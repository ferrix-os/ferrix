#!/usr/bin/env python3
"""Pack the corresponding source of every GPL and LGPL program a release ships.

    tools/common/release/package-sources.py <out-dir> --tag TAG [--fetch]

Writes `ferrix-<TAG>-sources.tar.gz` into <out-dir>: for each component
tools/common/data/release-licences.toml's `[sources]` names -- the
copyleft programs in the release's images, and what is linked statically
into one -- the exact upstream tarballs its build fetched, read from the
pins in its build script (pinned.py) and checked against them, its build
script and recipe with every patch it applies, and for busybox the config
it is built with; the C library they are all linked against, ferrousli,
at the commit components.toml pins; the source of each MPL-2.0 crate in
the inventory; the package list of the Pixel 7 edition's Debian Chromium
volume, with where each package's source is; and SOURCES.md, which says
how each program is rebuilt from what is here.

Tarballs come from the build's own download cache, where the release job
has just built them; `--fetch` downloads any that are not there, from the
URL its script pins, and checks it the same way. Entries are sorted and
carry no time or owner, so the same inputs give the same archive.
"""

from __future__ import annotations

import argparse
import gzip
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
import pinned  # noqa: E402

ROOT = pinned.ROOT
RULES = tomllib.loads((ROOT / "tools" / "common" / "data" / "release-licences.toml").read_text())
CHROMIUM = ROOT / "tools" / "common" / "fetch" / "fetch-chromium-arm64.sh"
BUSYBOX = pinned.FERROUSLI / "tools" / "busybox"
DEB = re.compile(r'^\s*"(main|updates|security) (pool/\S+/([^/]+)/(([^_/]+)_([^_]+)_([a-z0-9]+)\.deb)) ([0-9a-f]{64})"')


class Archive:
    """A tar written in name order, with no times, owners or modes but 0644/0755."""

    def __init__(self, top: str):
        self.top = top
        self.members: dict[str, tuple[bytes | pathlib.Path, int]] = {}

    def add_bytes(self, name: str, data: bytes, mode: int = 0o644) -> None:
        self.members[f"{self.top}/{name}"] = (data, mode)

    def add_file(self, name: str, path: pathlib.Path) -> None:
        mode = 0o755 if os.access(path, os.X_OK) else 0o644
        self.members[f"{self.top}/{name}"] = (path, mode)

    def add_tree(self, name: str, directory: pathlib.Path) -> None:
        for path in sorted(directory.rglob("*")):
            if path.is_file():
                self.add_file(f"{name}/{path.relative_to(directory)}", path)

    def write(self, target: pathlib.Path) -> None:
        with target.open("wb") as raw, gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as packed:
            with tarfile.open(fileobj=packed, mode="w", format=tarfile.PAX_FORMAT) as archive:
                for name in sorted(self.members):
                    content, mode = self.members[name]
                    info = tarfile.TarInfo(name)
                    info.mode, info.mtime, info.uid, info.gid, info.uname, info.gname = mode, 0, 0, 0, "", ""
                    if isinstance(content, bytes):
                        info.size = len(content)
                        archive.addfile(info, io.BytesIO(content))
                    else:
                        info.size = content.stat().st_size
                        with content.open("rb") as file:
                            archive.addfile(info, file)


def tarballs(archive: Archive, part: str, script: pathlib.Path, fetch_into: pathlib.Path | None) -> list[pinned.Download]:
    downloads = pinned.downloads(script)
    if not downloads:
        sys.exit(f"package-sources: {script} fetches nothing this can read")
    for download in downloads:
        archive.add_file(f"{part}/{download.file}", pinned.local(download, fetch_into))
    return downloads


def recipe(archive: Archive, part: str, directory: pathlib.Path) -> None:
    for path in sorted(directory.iterdir()):
        if path.is_file() and path.suffix not in (".png",):
            archive.add_file(f"{part}/recipe/{path.name}", path)
        elif path.is_dir():
            archive.add_tree(f"{part}/recipe/{path.name}", path)


def busybox(archive: Archive, fetch_into: pathlib.Path | None) -> list[str]:
    downloads = tarballs(archive, "busybox", pinned.BUSYBOX_SOURCES, fetch_into)
    recipe(archive, "busybox", BUSYBOX)
    # The config build.sh feeds `make oldconfig`: Alpine's, with config.sh's
    # changes, made the way build.sh makes it.
    alpine = pinned.local(next(d for d in downloads if d.file == "busyboxconfig"), fetch_into)
    changed = subprocess.run(
        ["bash", "-c", '. "$1"; c=$(mktemp); cp "$2" "$c"; apply_config_changes "$c"; cat "$c"; rm -f "$c"', "-",
         str(BUSYBOX / "config.sh"), str(alpine)],
        check=True, capture_output=True,
    ).stdout
    archive.add_bytes("busybox/busybox.config", changed)
    notes = ["`busybox.config` is Alpine's `busyboxconfig` with `recipe/config.sh`'s changes."]
    # And the .config `make oldconfig` turned that into, where this machine built it.
    built = pathlib.Path(os.environ.get("FERRIX_BUSYBOX", pathlib.Path.home() / ".local/share/ferrix/busybox/ferrousli"))
    for directory, arch in (("build", "x86_64"), ("build-aarch64", "aarch64"), ("build-armv7a", "armv7a")):
        config = built / directory / ".config"
        if config.is_file():
            archive.add_file(f"busybox/built/{arch}.config", config)
            notes.append(f"`built/{arch}.config` is the .config the {arch} build ran `make` with.")
    return notes


def ferrousli(archive: Archive) -> str:
    commit = subprocess.run(["git", "-C", str(pinned.FERROUSLI), "rev-parse", "HEAD"], check=True, capture_output=True, text=True).stdout.strip()
    pins = tomllib.loads((ROOT / "components.toml").read_text())["ferrousli"]["commit"]
    if commit != pins:
        sys.exit(f"package-sources: ferrousli's checkout is at {commit}, components.toml pins {pins}")
    data = subprocess.run(
        ["git", "-C", str(pinned.FERROUSLI), "archive", "--format=tar.gz", f"--prefix=ferrousli-{commit[:12]}/", commit],
        check=True, capture_output=True,
    ).stdout
    archive.add_bytes(f"ferrousli/ferrousli-{commit[:12]}.tar.gz", data)
    return commit


def mpl_crates(archive: Archive, inventory: pathlib.Path | None) -> list[str]:
    """The .crate of each MPL-2.0 crate the inventory lists: MPL's source is
    owed file by file, and these files are the crate."""
    if inventory is None:
        return []
    listed = []
    cache = pathlib.Path(os.environ.get("CARGO_HOME", pathlib.Path.home() / ".cargo")) / "registry" / "cache"
    for crate in json.loads(inventory.read_text())["crates"]:
        if "MPL-2.0" not in crate["licence"] or crate["kind"] != "crates.io":
            continue
        name = f"{crate['name']}-{crate['version']}.crate"
        found = next(iter(sorted(cache.glob(f"*/{name}"))), None)
        if found is None:
            sys.exit(f"package-sources: {name} is not in cargo's cache")
        archive.add_file(f"mpl-crates/{name}", found)
        listed.append(name)
    return listed


def debian(archive: Archive) -> int:
    """The Pixel 7 `full` edition's Chromium volume: its packages, where each
    one's source is kept, and the snapshot the volume was resolved against."""
    text = CHROMIUM.read_text()
    snapshot = re.search(r"^snapshot_time=(\S+)", text, re.M).group(1)
    rows = ["binary\tversion\tarch\tsource\tsource version\tsha256\tsource on snapshot.debian.org"]
    for line in text.splitlines():
        match = DEB.match(line)
        if not match:
            continue
        _, path, source, file, binary, version, arch, digest = match.groups()
        version = version.replace("%3a", ":")
        source_version = re.sub(r"\+b[0-9]+$", "", version)
        rows.append(
            f"{binary}\t{version}\t{arch}\t{source}\t{source_version}\t{digest}\t"
            f"https://snapshot.debian.org/package/{source}/{source_version}/"
        )
    archive.add_bytes("pixel7-debian/packages.tsv", ("\n".join(rows) + "\n").encode())
    archive.add_file("pixel7-debian/fetch-chromium-arm64.sh", CHROMIUM)
    archive.add_bytes(
        "pixel7-debian/README.md",
        (
            "# The Pixel 7 `full` edition's Chromium volume\n\n"
            "`ferrix-pixel7-chromium.img.gz` is Debian 13's Chromium for arm64 and every library it\n"
            "loads, unpacked unchanged from Debian's own .deb files onto a btrfs volume by\n"
            "`fetch-chromium-arm64.sh` (here, as it was at the tag). `packages.tsv` lists each package,\n"
            "its version and SHA-256 as the script pins it, and its source package. The archive was\n"
            f"resolved against trixie, trixie-updates and trixie-security as of {snapshot}; each\n"
            "source package, with Debian's changes and copyright file, is kept at the\n"
            "snapshot.debian.org address in the last column, and Debian's own licence for each\n"
            "package is its `usr/share/doc/<package>/copyright`, which the volume carries.\n\n"
            "This release points to those sources rather than carrying them: Chromium's alone is\n"
            "over a gigabyte. docs/LICENSES.md says what that leaves open.\n"
        ).encode(),
    )
    return len(rows) - 1


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("out", type=pathlib.Path)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--fetch", action="store_true", help="download what the build's cache lacks")
    parser.add_argument("--inventory", type=pathlib.Path, help="licences.json, for the MPL-2.0 crates")
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    fetch_into = args.out / "downloads" if args.fetch else None
    top = f"ferrix-{args.tag}-sources"
    archive = Archive(top)
    sections = []

    notes = busybox(archive, fetch_into)
    sections.append(("busybox", "GPL-2.0-only", "both editions of the Pixel 7 desktop and the x86-64 desktop",
                     "`recipe/build.sh` from ferrousli's checkout (`tools/busybox/`), after `cargo xtask busybox`'s ferrousli build: "
                     + " ".join(notes)))
    apps = {path.parent.name: tomllib.loads(path.read_text()) for path in pinned.APPS.glob("*/app.toml")}
    for name in sorted(RULES["sources"]):
        if name in ("busybox", "ferrousli") or name not in apps and not pinned.port_script(name).is_file():
            continue
        if name in apps:
            tarballs(archive, name, pinned.app_script(name), fetch_into)
            recipe(archive, name, pinned.APPS / name)
            depends = apps[name]["package"].get("depends", [])
            ports = pinned.ports_linked(name)
            how = (f"`recipe/build.sh <arch> <out>` from the apps checkout (`src/user/apps/{name}/`), "
                   f"as `cargo xtask build-apps --app {name}` runs it" + (f"; built after {', '.join(depends)}" if depends else "")
                   + (f"; links ferrousli's {', '.join(ports)} port" if ports else ""))
            sections.append((name, apps[name]["package"]["license"], "the x86-64 desktop", how))
        else:
            tarballs(archive, name, pinned.port_script(name), fetch_into)
            recipe(archive, name, pinned.PORTS / name)
            sections.append((name, "see the inventory", "linked into the programs above",
                             f"`recipe/build.sh --arch <arch>` from ferrousli's checkout (`tools/ports/{name}/`)"))
    archive.add_file("common/ports-common.sh", pinned.PORTS / "common.sh")
    commit = ferrousli(archive)
    crates = mpl_crates(archive, args.inventory)
    packages = debian(archive)

    lines = [
        f"# Ferrix {args.tag}: corresponding sources",
        "",
        "The source of every program under the GPL or the LGPL in this release's assets, and of",
        "what is linked statically into one, as each was built. Every tarball is the upstream",
        "release its build script fetches, checked against the SHA-256 (or SHA-512) pinned in that",
        "script, which is in `recipe/` beside it with every patch it applies. The licence of every",
        "part of the release is in its THIRD-PARTY-LICENSES.md.",
        "",
        "Each program is built against ferrousli, Ferrix's C library, linked statically: its source",
        f"is `ferrousli/ferrousli-{commit[:12]}.tar.gz` (ferrix-os/ferrousli at {commit}), and the scripts",
        "source its port toolkit, `common/ports-common.sh` (its `tools/ports/common.sh`). They run on a",
        "Linux host with a C compiler; the Ferrix repository at the tag runs them for you:",
        "",
        "```",
        f"git clone https://github.com/ferrix-os/ferrix && cd ferrix && git checkout {args.tag}",
        "cargo xtask busybox --arch x86_64        # and --arch aarch64 for the Pixel 7's",
        "cargo xtask ports --arch x86_64",
        "cargo xtask build-apps --arch x86_64",
        "```",
        "",
        "and put a tarball from here in `~/.local/share/ferrix/ports/ferrousli/src/` (busybox's in",
        "`~/.local/share/ferrix/busybox/ferrousli/src/`) to build from it rather than a download.",
        "",
        "| Part | Licence | Shipped in | How it is built |",
        "|---|---|---|---|",
        *[f"| {name} | {licence} | {where} | {how} |" for name, licence, where, how in sections],
        "",
        "Why each is here: " + "; ".join(f"{name}: {why}" for name, why in sorted(RULES["sources"].items())) + ".",
        "",
        "alsa-lib's static library is linked into alsa-utils' `aplay` and `speaker-test` only, both",
        "GPL-2.0-or-later, whose source is here; the alsa-lib package itself puts only its",
        "configuration files on the image. No program under another licence links it.",
        "",
        f"`mpl-crates/`: the {len(crates)} MPL-2.0 Rust crates the inventory lists, as crates.io serves them.",
        "",
        f"`pixel7-debian/`: the {packages} Debian packages on the Pixel 7 `full` edition's Chromium volume,",
        "and where each one's source is (its README).",
        "",
    ]
    archive.add_bytes("SOURCES.md", "\n".join(lines).encode())
    target = args.out / f"{top}.tar.gz"
    archive.write(target)
    print(f"{target}: {target.stat().st_size} bytes, {len(archive.members)} files")


if __name__ == "__main__":
    main()
