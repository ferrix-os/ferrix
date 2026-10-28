#!/usr/bin/env python3
"""Write a release's licence inventory, and refuse a release it cannot account for.

    tools/common/release/licences.py --initrd <stage>/ferrix-x86_64.initrd --out <dir> [--tag TAG]

Writes into <dir>:

    THIRD-PARTY-LICENSES.md   what the image is made of, by licence, with every text
    licences.json             the same, for a program to read

and exits non-zero, after printing every reason, when the release ships
something it cannot name the licence of (docs/LICENSES.md):

  * a file in the image's initramfs that no package record owns and no rule
    in tools/common/data/release-licences.toml names;
  * an app, a component or a Rust crate with no licence, or one whose licence
    is outside that file's `permissive` and `copyleft` lists;
  * a copyleft component the release's sources asset does not carry
    (`[sources]` there), which would ship a binary with no source offered.

Everything is read, not written down: the apps' licences from their
app.toml, each crate's from its own Cargo.toml in cargo's registry, the C
programs' upstream tarballs from the pins their build scripts fetch by
(pinned.py), their licence texts out of those tarballs, and the file list
from the image itself. For an `A OR B` it takes the first alternative that
is permissive, and says which it took.

Needs the crates of every workspace fetched (it runs `cargo fetch --locked`
where one is missing) and the C programs' tarballs where their builds keep
them: run it after the build, as the release workflow does.
"""

from __future__ import annotations

import argparse
import fnmatch
import hashlib
import io
import json
import os
import pathlib
import re
import subprocess
import sys
import tarfile
import tomllib

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import cpio  # noqa: E402
import pinned  # noqa: E402

ROOT = pinned.ROOT
DATA = ROOT / "tools" / "common" / "data" / "release-licences.toml"
CARGO_HOME = pathlib.Path(os.environ.get("CARGO_HOME", pathlib.Path.home() / ".cargo"))
LICENCE_FILE = re.compile(r"^(licen[cs]e|copying|notice|copyright|unlicense|authors?\.license)([-_.].*)?$", re.I)
# Where a workspace of programs for Ferrix lives. src/tests holds test
# programs no release image carries; the Pixel 7's monitor is a host tool.
WORKSPACES = ["Cargo.lock", "src/user/system/linux/**/Cargo.lock", "src/user/apps/*/Cargo.lock"]


# --- SPDX ------------------------------------------------------------------


def tokens(expression: str) -> list[str]:
    # Old crates write `MIT/Apache-2.0` for an OR.
    expression = expression.replace("/", " OR ")
    return re.findall(r"\(|\)|[A-Za-z0-9.+:-]+", expression)


def parse(expression: str):
    """An SPDX expression as nested ("AND"|"OR", [...]) and identifier strings."""
    items = tokens(expression)
    position = 0

    def atom():
        nonlocal position
        if position >= len(items):
            raise ValueError(f"unfinished licence expression {expression!r}")
        token = items[position]
        position += 1
        if token == "(":
            inner = disjunction()
            position += 1  # ")"
            return inner
        if position < len(items) and items[position] == "WITH":
            position += 1
            token = f"{token} WITH {items[position]}"
            position += 1
        return token

    def conjunction():
        nonlocal position
        parts = [atom()]
        while position < len(items) and items[position] == "AND":
            position += 1
            parts.append(atom())
        return parts[0] if len(parts) == 1 else ("AND", parts)

    def disjunction():
        nonlocal position
        parts = [conjunction()]
        while position < len(items) and items[position] == "OR":
            position += 1
            parts.append(conjunction())
        return parts[0] if len(parts) == 1 else ("OR", parts)

    tree = disjunction()
    if position != len(items):
        raise ValueError(f"cannot read licence expression {expression!r}")
    return tree


def identifiers(tree) -> list[str]:
    if isinstance(tree, str):
        return [tree]
    return [one for part in tree[1] for one in identifiers(part)]


def choose(tree, permissive: set[str]):
    """The licence taken: every AND kept, of each OR the first alternative
    that is wholly permissive, else the first."""
    if isinstance(tree, str):
        return tree
    kind, parts = tree
    chosen = [choose(part, permissive) for part in parts]
    if kind == "AND":
        return ("AND", chosen)
    for part in chosen:
        if all(one in permissive for one in identifiers(part)):
            return part
    return chosen[0]


def render(tree) -> str:
    if isinstance(tree, str):
        return tree
    kind, parts = tree
    return f" {kind} ".join(render(p) if isinstance(p, str) else f"({render(p)})" for p in parts)


# --- What is read ------------------------------------------------------------


class Inventory:
    def __init__(self, rules: dict):
        self.rules = rules
        self.permissive = set(rules["permissive"])
        self.copyleft = set(rules["copyleft"])
        self.problems: list[str] = []
        self.texts: dict[str, tuple[str, str]] = {}  # digest -> (title, text)
        self.text_users: dict[str, list[str]] = {}

    def judge(self, who: str, expression: str | None, sourced: bool = False) -> str:
        """`expression` as taken, and a problem recorded when it may not ship."""
        if not expression:
            self.problems.append(f"{who}: no licence")
            return "NONE"
        try:
            tree = parse(expression)
        except ValueError as error:
            self.problems.append(f"{who}: {error}")
            return expression
        taken = choose(tree, self.permissive)
        for one in identifiers(taken):
            if one in self.copyleft:
                if not sourced:
                    self.problems.append(
                        f"{who}: {one}, a copyleft licence, and the sources asset does not carry it"
                    )
            elif one not in self.permissive:
                self.problems.append(f"{who}: {one} is not a licence release-licences.toml allows")
        return render(taken)

    def text(self, title: str, data: bytes, user: str) -> str:
        text = data.decode("utf-8", "replace").replace("\r\n", "\n").strip()
        digest = hashlib.sha256(text.encode()).hexdigest()[:16]
        self.texts.setdefault(digest, (title, text))
        self.text_users.setdefault(digest, []).append(user)
        return digest


def lockfiles() -> list[pathlib.Path]:
    found: set[pathlib.Path] = set()
    for pattern in WORKSPACES:
        found.update(ROOT.glob(pattern))
    return sorted(path for path in found if "/target/" not in str(path) and "/build/" not in str(path))


def own_manifests() -> dict[str, list[pathlib.Path]]:
    """Every Cargo.toml in the tree by its package name: Ferrix's own crates."""
    found: dict[str, list[pathlib.Path]] = {}
    for manifest in ROOT.glob("**/Cargo.toml"):
        text = str(manifest)
        if "/target/" in text or "/build/" in text or "/.claude/" in text[len(str(ROOT)) :]:
            continue
        try:
            package = tomllib.loads(manifest.read_text()).get("package")
        except (tomllib.TOMLDecodeError, UnicodeDecodeError):
            continue
        if package and isinstance(package.get("name"), str):
            found.setdefault(package["name"], []).append(manifest)
    return found


def workspace_licence(manifest: pathlib.Path) -> str | None:
    for parent in manifest.parents:
        root = parent / "Cargo.toml"
        if root.is_file():
            data = tomllib.loads(root.read_text())
            licence = data.get("workspace", {}).get("package", {}).get("license")
            if licence:
                return licence
        if parent == ROOT:
            break
    return None


def manifest_licence(manifest: pathlib.Path) -> str | None:
    package = tomllib.loads(manifest.read_text()).get("package", {})
    licence = package.get("license")
    if isinstance(licence, dict) and licence.get("workspace"):
        return workspace_licence(manifest)
    return licence


def registry_dir(name: str, version: str) -> pathlib.Path | None:
    for index in sorted((CARGO_HOME / "registry" / "src").glob("*")):
        candidate = index / f"{name}-{version}"
        if (candidate / "Cargo.toml").is_file():
            return candidate
    return None


def git_dir(name: str, source: str) -> pathlib.Path | None:
    commit = source.rsplit("#", 1)[-1]
    for manifest in (CARGO_HOME / "git" / "checkouts").glob(f"*/{commit[:7]}*/**/Cargo.toml"):
        try:
            package = tomllib.loads(manifest.read_text()).get("package", {})
        except (tomllib.TOMLDecodeError, UnicodeDecodeError):
            continue
        if package.get("name") == name:
            return manifest.parent
    return None


def crate_texts(directory: pathlib.Path) -> list[pathlib.Path]:
    return sorted(path for path in directory.iterdir() if path.is_file() and LICENCE_FILE.match(path.name))


def crates(inventory: Inventory, extra_locks: list[pathlib.Path]) -> list[dict]:
    own = own_manifests()
    seen: dict[tuple[str, str, str], dict] = {}
    for lock in lockfiles() + extra_locks:
        workspace = str(lock.parent.relative_to(ROOT)) if lock.is_relative_to(ROOT) else lock.parent.name
        packages = tomllib.loads(lock.read_text()).get("package", [])
        fetched = False
        for package in packages:
            name, version, source = package["name"], package["version"], package.get("source", "")
            key = (name, version, source)
            if key in seen:
                seen[key]["workspaces"].add(workspace)
                continue
            entry = {"name": name, "version": version, "workspaces": {workspace}}
            if not source:
                manifests = own.get(name, [])
                # A crate a script app builds from crates.io (sshdt), whose
                # lockfile is its own: its Cargo.toml is beside the lock.
                beside = lock.parent / "Cargo.toml"
                if not manifests and beside.is_file() and tomllib.loads(beside.read_text()).get("package", {}).get("name") == name:
                    manifests = [beside]
                entry["kind"] = "own"
                expression = manifest_licence(manifests[0]) if manifests else None
                if not manifests:
                    inventory.problems.append(f"crate {name} {version} ({workspace}): a path crate this cannot find")
                entry["licence"] = inventory.judge(f"crate {name} {version}", expression)
                seen[key] = entry
                continue
            # A crate of a workspace this machine never built is fetched,
            # once per workspace: from crates.io, or a git dependency's
            # checkout (media's pulseaudio-rs, on a release runner).
            if source.startswith("registry+"):
                find = lambda: registry_dir(name, version)  # noqa: E731
                entry["kind"] = "crates.io"
            else:
                find = lambda: git_dir(name, source)  # noqa: E731
                entry["kind"] = source.split("#")[0].removeprefix("git+")
            directory = find()
            if directory is None and not fetched:
                print(f"cargo fetch --locked in {workspace}", flush=True)
                subprocess.run(
                    ["cargo", "fetch", "--locked", "--manifest-path", str(lock.parent / "Cargo.toml")],
                    check=False,
                )
                fetched = True
                directory = find()
            if directory is None:
                inventory.problems.append(f"crate {name} {version} ({source}): not in cargo's cache to read")
                entry["licence"] = "UNREAD"
                seen[key] = entry
                continue
            manifest = tomllib.loads((directory / "Cargo.toml").read_text()).get("package", {})
            expression = manifest.get("license")
            if not expression and manifest.get("license-file"):
                expression = "LicenseRef-file"
                inventory.problems.append(
                    f"crate {name} {version}: only a license-file ({manifest['license-file']}), to be read and listed by hand"
                )
            entry["licence"] = inventory.judge(f"crate {name} {version}", expression)
            entry["declared"] = expression
            entry["texts"] = [
                inventory.text(f"{name} {version}: {path.name}", path.read_bytes(), f"{name} {version}")
                for path in crate_texts(directory)
            ]
            seen[key] = entry
    for entry in seen.values():
        entry["workspaces"] = sorted(entry["workspaces"])
    return sorted(seen.values(), key=lambda entry: (entry["kind"] != "own", entry["name"], entry["version"]))


def tarball_texts(inventory: Inventory, path: pathlib.Path, user: str) -> list[str]:
    """The licence texts at the top of an upstream tarball, two levels deep."""
    found = []
    with tarfile.open(path) as archive:
        for member in archive:
            parts = member.name.strip("./").split("/")
            if not member.isfile() or len(parts) > 3:
                continue
            name = parts[-1]
            if LICENCE_FILE.match(name) or (len(parts) == 3 and parts[1] == "docs" and name == "license.rst"):
                data = archive.extractfile(member).read()
                found.append(inventory.text(f"{user}: {'/'.join(parts[1:])}", data, user))
    return found


def apps(inventory: Inventory, present: set[str]) -> list[dict]:
    """The apps whose files are in the image."""
    sources = inventory.rules["sources"]
    listed = []
    for manifest in sorted(pinned.APPS.glob("*/app.toml")):
        recipe = tomllib.loads(manifest.read_text())
        package = recipe["package"]
        name = package["name"]
        if name not in present:
            continue
        entry = {
            "name": name,
            "version": package["version"],
            "declared": package.get("license"),
            "licence": inventory.judge(f"app {name}", package.get("license"), name in sources),
            "build": recipe.get("build", {}).get("kind", "cargo"),
            "upstream": [],
            # The licence files in the app's own folder: a font it embeds,
            # say (term's Hack). An app is named only from inside its folder
            # (docs/APPS.md §4), so they are found here, not listed elsewhere.
            "texts": [
                inventory.text(f"{name}: {path.name}", path.read_bytes(), f"app {name}")
                for path in crate_texts(manifest.parent)
            ],
        }
        if entry["build"] == "script":
            for download in pinned.downloads(pinned.app_script(name)):
                item = {"file": download.file, "url": download.url, download.algorithm: download.digest}
                if not download.file.endswith((".pem", ".crt")):
                    item["texts"] = tarball_texts(inventory, pinned.local(download), f"{name}: {download.file}")
                entry["upstream"].append(item)
            entry["ports"] = pinned.ports_linked(name)
        listed.append(entry)
    return listed


def components(inventory: Inventory, present: set[str]) -> list[dict]:
    """The components in the image: those a file of it is, and the ports an
    app in it links."""
    listed = []
    sources = inventory.rules["sources"]
    for name, component in inventory.rules["components"].items():
        if name not in present:
            continue
        entry = {
            "name": name,
            "licence": inventory.judge(f"component {name}", component.get("licence"), name in sources),
            "upstream": component.get("upstream"),
            "note": component.get("note", ""),
            "pins": [],
        }
        if component.get("pinned"):
            for download in pinned.downloads(ROOT / component["pinned"]):
                item = {"file": download.file, "url": download.url, download.algorithm: download.digest}
                if download.file != "busyboxconfig":
                    item["texts"] = tarball_texts(inventory, pinned.local(download), f"{name}: {download.file}")
                entry["pins"].append(item)
        listed.append(entry)
    for name, repo in (("ferrousli", "src/user/system/linux/ferrousli"), ("zinc", "src/user/system/linux/zinc"), ("apps", "src/user/apps"), ("ferrix", ".")):
        licence = ROOT / repo / "LICENSE"
        if licence.is_file():
            inventory.text(f"{name}: LICENSE", licence.read_bytes(), name)
    for embedded in inventory.rules.get("embedded", []):
        inventory.judge(f"embedded {embedded['what']}", embedded["licence"])
        embedded["text"] = inventory.text(embedded["what"], (ROOT / embedded["file"]).read_bytes(), embedded["what"])
    return listed


def image_files(inventory: Inventory, initrd: pathlib.Path, app_names: set[str]) -> tuple[dict[str, int], list[pathlib.Path]]:
    """How many regular files of the image each app or component is."""
    entries = cpio.entries(initrd.read_bytes())
    owner: dict[str, str] = {}
    for entry in entries:
        if entry.name.startswith("lib/ferrix/packages/") and entry.name.endswith(".toml"):
            record = tomllib.loads(entry.data.decode())
            for file in record.get("files", []):
                owner[file["path"]] = f"app {record['package']['name']}"
            owner[entry.name] = f"app {record['package']['name']}"
    rules = [(pattern, f"component {name}") for name, c in inventory.rules["components"].items() for pattern in c.get("paths", [])]
    counts: dict[str, int] = {}
    for entry in entries:
        if not entry.regular:
            continue  # directories and links carry no work of their own
        who = owner.get(entry.name)
        if who is None and entry.name.startswith("bin/") and entry.name[4:] in app_names:
            who = f"app {entry.name[4:]}"
        if who is None:
            who = next((who for pattern, who in rules if fnmatch.fnmatchcase(entry.name, pattern)), None)
        if who is None:
            inventory.problems.append(f"image file /{entry.name}: no package record owns it and no rule names it")
            who = "UNKNOWN"
        counts[who] = counts.get(who, 0) + 1
    return counts, []


def crate_locks(present: set[str], scratch: pathlib.Path) -> list[pathlib.Path]:
    """The Cargo.lock inside each .crate a script app in the image builds
    from (sshdt's), with a Cargo.toml beside it to fetch by."""
    locks = []
    for script in sorted(pinned.APPS.glob("*/build.sh")):
        if script.parent.name not in present:
            continue
        for download in pinned.downloads(script):
            if not download.file.endswith(".crate"):
                continue
            target = scratch / download.file.removesuffix(".crate")
            with tarfile.open(pinned.local(download)) as archive:
                for member in archive:
                    if member.name.count("/") == 1 and member.name.split("/")[1] in ("Cargo.lock", "Cargo.toml"):
                        target.mkdir(parents=True, exist_ok=True)
                        (target / member.name.split("/")[1]).write_bytes(archive.extractfile(member).read())
            if (target / "Cargo.lock").is_file():
                # A workspace of its own: the scratch directory may sit
                # inside this tree (the live ISO's is under build/), and
                # cargo would otherwise take the tree's workspace for its
                # own and refuse to fetch -- which a machine with every
                # crate cached never notices, and a release runner does.
                manifest = target / "Cargo.toml"
                text = manifest.read_text()
                if "workspace" not in tomllib.loads(text):
                    manifest.write_text(text + "\n[workspace]\n")
                (target / "src").mkdir(exist_ok=True)
                (target / "src" / "main.rs").write_text("fn main() {}\n")
                locks.append(target / "Cargo.lock")
    return locks


# --- What is written ---------------------------------------------------------


def summary(apps_listed, components_listed, crates_listed) -> dict[str, int]:
    counts: dict[str, int] = {}
    for entry in [*apps_listed, *components_listed, *crates_listed]:
        try:
            ids = identifiers(parse(entry["licence"]))
        except ValueError:
            ids = [entry["licence"]]
        for one in sorted(set(ids)):
            counts[one] = counts.get(one, 0) + 1
    return dict(sorted(counts.items(), key=lambda item: (-item[1], item[0])))


def write(out: pathlib.Path, inventory: Inventory, data: dict) -> None:
    out.mkdir(parents=True, exist_ok=True)
    (out / "licences.json").write_text(json.dumps(data, indent=1, sort_keys=True) + "\n")
    lines = [
        f"# Third-party licences: Ferrix {data['tag']}, {data['title']}",
        "",
        f"Generated by `tools/common/release/licences.py` from commit {data['commit']}; not edited by",
        "hand. What the release image is made of, the licence each part is shipped under, and",
        "every licence text. The source of each GPL and LGPL program, and of what is linked into",
        f"one, is the release's sources asset, `ferrix-{data['tag']}-sources.tar.gz`; docs/LICENSES.md says how",
        "the inventory and the sources are made.",
        "",
        "Where a part offers a choice (`A OR B`), the licence below is the one Ferrix takes.",
        "",
        "Firmware: none is bundled. The run scripts boot the UEFI firmware (OVMF) your QEMU or",
        "your distribution installed, under its own licence.",
        "",
        "## Summary",
        "",
        "Parts by licence (a part under `A AND B` counts once for each):",
        "",
        "| Licence | Parts |",
        "|---|---|",
        *[f"| {name} | {count} |" for name, count in data["summary"].items()],
        "",
        f"{len(data['crates'])} Rust crates ({sum(c['kind'] == 'own' for c in data['crates'])} of them Ferrix's own), "
        f"{len(data['apps'])} apps, {len(data['components'])} other components; "
        f"{sum(data['image_files'].values())} regular files in the image's initramfs.",
        "",
        "## Ferrix's own",
        "",
        "The kernel, the loader, the drivers, the system's programs, ferrousli (the C library),",
        "zinc (the shell) and the apps written for Ferrix are MIT licensed, each repository with its",
        "own LICENSE (texts below). Third-party material inside those sources:",
        "",
        *[f"- {e['what']}: {e['licence']} (`{e['file']}`, text {e['text']})" for e in data["embedded"]],
        "",
        "## Apps",
        "",
        "| App | Version | Licence (as taken) | Declared | Built from |",
        "|---|---|---|---|---|",
    ]
    for app in data["apps"]:
        built = "<br>".join(f"[{u['file']}]({u['url']})" for u in app["upstream"]) or "this tree"
        if app.get("ports"):
            built += "<br>ports: " + ", ".join(app["ports"])
        lines.append(f"| {app['name']} | {app['version']} | {app['licence']} | {app['declared']} | {built} |")
    lines += ["", "## Other components", "", "| Component | Licence | Upstream | Pinned sources | Note |", "|---|---|---|---|---|"]
    for component in data["components"]:
        pins = "<br>".join(f"[{p['file']}]({p['url']})" for p in component["pins"]) or "-"
        lines.append(f"| {component['name']} | {component['licence']} | {component['upstream']} | {pins} | {component['note']} |")
    lines += ["", "## Files in the image", "", "| Part | Regular files |", "|---|---|"]
    lines += [f"| {who} | {count} |" for who, count in sorted(data["image_files"].items())]
    lines += ["", "## Rust crates", "", "| Crate | Version | Licence (as taken) | From | Texts |", "|---|---|---|---|---|"]
    for crate in data["crates"]:
        texts = ", ".join(crate.get("texts", [])) or ("the repository's LICENSE" if crate["kind"] == "own" else "none in the crate: the licence's standard text")
        lines.append(f"| {crate['name']} | {crate['version']} | {crate['licence']} | {crate['kind']} | {texts} |")
    lines += ["", "## Licence texts", "", "Each text once, under the id the tables above give it.", ""]
    for digest, (title, text) in sorted(inventory.texts.items(), key=lambda item: item[1][0].lower()):
        users = sorted(set(inventory.text_users[digest]))
        shown = ", ".join(users[:12]) + (f" and {len(users) - 12} more" if len(users) > 12 else "")
        lines += [f"### {digest}: {title}", "", f"Used by: {shown}", "", "```text", text.replace("```", "'''"), "```", ""]
    (out / "THIRD-PARTY-LICENSES.md").write_text("\n".join(lines))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--initrd", type=pathlib.Path, required=True)
    parser.add_argument("--out", type=pathlib.Path, required=True)
    parser.add_argument("--tag", default="untagged")
    parser.add_argument("--title", default="x86-64 desktop", help="which image, for the heading")
    args = parser.parse_args()
    inventory = Inventory(tomllib.loads(DATA.read_text()))
    every_app = {path.parent.name for path in pinned.APPS.glob("*/app.toml")}
    files, _ = image_files(inventory, args.initrd, every_app)
    present_apps = {who.removeprefix("app ") for who in files if who.startswith("app ")}
    present = {who.removeprefix("component ") for who in files if who.startswith("component ")}
    present.update(port for app in present_apps if pinned.app_script(app).is_file() for port in pinned.ports_linked(app))
    apps_listed = apps(inventory, present_apps)
    components_listed = components(inventory, present)
    crates_listed = crates(inventory, crate_locks(present_apps, args.out / ".crates"))
    commit = subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT, capture_output=True, text=True).stdout.strip()
    data = {
        "title": args.title,
        "tag": args.tag,
        "commit": commit,
        "apps": apps_listed,
        "components": components_listed,
        "embedded": inventory.rules.get("embedded", []),
        "crates": crates_listed,
        "image_files": files,
        "summary": summary(apps_listed, components_listed, crates_listed),
        "problems": inventory.problems,
    }
    write(args.out, inventory, data)
    for name, count in data["summary"].items():
        print(f"  {count:5d}  {name}")
    print(f"licences: {len(crates_listed)} crates, {len(apps_listed)} apps, {len(components_listed)} components, "
          f"{sum(files.values())} image files, {len(inventory.texts)} distinct texts -> {args.out}")
    if inventory.problems:
        for problem in inventory.problems:
            print(f"licences: REFUSED: {problem}", file=sys.stderr)
        sys.exit(1)
    print("licences: every part of the image is accounted for")


if __name__ == "__main__":
    main()
