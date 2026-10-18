#!/usr/bin/env python3
"""Build and pack the x86-64 desktop a release carries for people with QEMU and nothing else.

    tools/common/release/package-desktop.py <out-dir> [--tag TAG] [--image ferrix-x86_64.img]

Builds the image with `cargo xtask release-image` (run-compositor's desktop,
anybody's rather than this machine's: tools/common/xtask/src/compositor/run.rs),
unless `--image` names one already built, and writes into `out-dir`:

    ferrix-x86_64-desktop.zip           the folder below, for every host
    ferrix-x86_64-desktop.zip.sha256    its SHA-256, as sha256sum prints it

    ferrix-x86_64-desktop/
        ferrix-x86_64.img   the UEFI disk: loader, kernel, initramfs
        run.sh              boots it with plain QEMU on Linux and macOS
        run.ps1             the same on Windows
        README.txt          what it is, what is left out, and why
        LICENSE             Ferrix's
        VERSION             the tag and the commit it was built from

A zip rather than a tarball because every host unpacks one with what it
already has: Explorer, macOS's Archive Utility, `unzip` or `bsdtar`. Every
entry carries the same time and the scripts their mode, so the same image
gives the same archive.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import pathlib
import subprocess
import sys
import zipfile

ROOT = pathlib.Path(__file__).resolve().parents[3]
HERE = pathlib.Path(__file__).resolve().parent / "desktop"
NAME = "ferrix-x86_64-desktop"
IMAGE = "ferrix-x86_64.img"
# 1980-01-01, the earliest time a zip entry has: no build's own time in it.
EPOCH = (1980, 1, 1, 0, 0, 0)


def build_image(stage: pathlib.Path) -> pathlib.Path:
    """`cargo xtask release-image`, into `stage`."""
    subprocess.run(
        ["cargo", "xtask", "release-image", "--arch", "x86_64", "--stage", str(stage)],
        cwd=ROOT,
        check=True,
    )
    return stage / IMAGE


def commit() -> str:
    """The commit this tree is at, and `-dirty` when it has changes."""
    head = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=ROOT, check=True, capture_output=True, text=True
    ).stdout.strip()
    dirty = subprocess.run(
        ["git", "status", "--porcelain", "--untracked-files=no"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    return head + ("-dirty" if dirty else "")


def entry(name: str, mode: int) -> zipfile.ZipInfo:
    """A zip entry with no time of its own and Unix permissions."""
    info = zipfile.ZipInfo(f"{NAME}/{name}", date_time=EPOCH)
    info.compress_type = zipfile.ZIP_DEFLATED
    info.create_system = 3  # Unix, so unzip and bsdtar honour the mode
    info.external_attr = (0o100000 | mode) << 16
    return info


def text(path: pathlib.Path) -> bytes:
    """A script or text file with the line ends its host wants: CRLF for
    run.ps1 and README.txt, which Windows opens, LF for the rest."""
    data = path.read_bytes().replace(b"\r\n", b"\n")
    if path.suffix in (".ps1", ".txt"):
        data = data.replace(b"\n", b"\r\n")
    return data


def pack(image: pathlib.Path, out: pathlib.Path, tag: str) -> pathlib.Path:
    archive = out / f"{NAME}.zip"
    version = f"tag {tag}\ncommit {commit()}\n"
    with zipfile.ZipFile(archive, "w", compresslevel=9) as zip_file:
        with image.open("rb") as source, zip_file.open(entry(IMAGE, 0o644), "w", force_zip64=True) as target:
            while chunk := source.read(1 << 20):
                target.write(chunk)
        zip_file.writestr(entry("run.sh", 0o755), text(HERE / "run.sh"))
        zip_file.writestr(entry("run.ps1", 0o644), text(HERE / "run.ps1"))
        zip_file.writestr(entry("README.txt", 0o644), text(HERE / "README.txt"))
        zip_file.writestr(entry("LICENSE", 0o644), (ROOT / "LICENSE").read_bytes())
        zip_file.writestr(entry("VERSION", 0o644), version.encode())
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    (out / f"{archive.name}.sha256").write_text(f"{digest}  {archive.name}\n")
    print(f"{archive}: {archive.stat().st_size} bytes, from {image.stat().st_size}; sha256 {digest}")
    return archive


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("out", type=pathlib.Path)
    parser.add_argument("--tag", default="untagged", help="the release's tag, for VERSION")
    parser.add_argument("--image", type=pathlib.Path, help="an image release-image already wrote")
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    image = args.image or build_image(args.out / "stage")
    if not image.is_file():
        sys.exit(f"package-desktop: no image at {image}")
    pack(image, args.out, args.tag)


if __name__ == "__main__":
    os.umask(0o022)
    main()
