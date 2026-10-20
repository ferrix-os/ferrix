"""The pinned downloads Ferrix's build scripts fetch, read out of the scripts.

Every C program an image carries is built by a shell script that fetches
its upstream tarballs through one `fetch` function, refusing a file whose
checksum differs from the one pinned beside it: the apps' `build.sh`
(src/user/apps/<name>/), ferrousli's ports (tools/ports/<port>/build.sh)
and its busybox (tools/busybox/sources.sh). Two shapes are used, and this
reads both:

    GIT_VERSION=2.55.0
    GIT_TARBALL=git-$GIT_VERSION.tar.xz
    GIT_URL=https://www.kernel.org/pub/software/scm/git/$GIT_TARBALL
    GIT_SHA256=457f...
    fetch "$GIT_URL" "$src/$GIT_TARBALL" sha256sum "$GIT_SHA256"

and a list of `"name version file url sha256"` lines, which a loop fetches.
A `fetch` may name mirrors after the upstream URL, in the same quoted
argument (`fetch "$ZLIB_URL $ZLIB_MIRROR" ...`), for an upstream that turns
some machines away -- GitHub's runners, which build a release, among them.
`url` is the upstream's, the one the licence inventory names; `local` tries
the mirrors after it, and checks what each gives against the one pin.

A `fetch` line whose arguments do not resolve is an error, never a skip:
the release's sources and licence inventory are only as complete as this
reading, so a script written in a third shape stops them until this learns
it. `licences.py` and `package-sources.py` use it; the build scripts stay
the one place a pin is written.
"""

from __future__ import annotations

import hashlib
import os
import pathlib
import re
import subprocess
from dataclasses import dataclass

ROOT = pathlib.Path(__file__).resolve().parents[3]
APPS = ROOT / "src" / "user" / "apps"
FERROUSLI = ROOT / "src" / "user" / "system" / "linux" / "ferrousli"
PORTS = FERROUSLI / "tools" / "ports"
BUSYBOX_SOURCES = FERROUSLI / "tools" / "busybox" / "sources.sh"

ASSIGNMENT = re.compile(r"^([A-Z_][A-Z0-9_]*)=(\S+)\s*$")
VARIABLE = re.compile(r"\$\{?([A-Za-z_][A-Za-z0-9_]*)\}?")
FETCH = re.compile(r'^\s*fetch\s+"([^"]+)"\s+"([^"]+)"\s+(sha256sum|sha512sum)\s+"([^"]+)"\s*$')
LISTED = re.compile(r'^\s*"(\S+) (\S+) (\S+) (https?://\S+) ([0-9a-f]{64})"\s*$')


@dataclass(frozen=True)
class Download:
    """One pinned file a build script fetches."""

    script: pathlib.Path
    file: str
    url: str
    algorithm: str  # "sha256" or "sha512"
    digest: str
    mirrors: tuple[str, ...] = ()

    @property
    def name(self) -> str:
        """The file's name without its version and suffixes: `git`, `mbedtls`."""
        return re.split(r"[-_][0-9]", self.file, maxsplit=1)[0]


def _expand(text: str, variables: dict[str, str]) -> str | None:
    missing = False

    def one(match: re.Match[str]) -> str:
        nonlocal missing
        value = variables.get(match.group(1))
        if value is None:
            missing = True
            return ""
        return value

    expanded = VARIABLE.sub(one, text.strip("\"'"))
    return None if missing else expanded


def downloads(script: pathlib.Path) -> list[Download]:
    """Every pinned download `script` fetches, in the order it fetches them."""
    variables: dict[str, str] = {}
    found: list[Download] = []
    for number, line in enumerate(script.read_text().splitlines(), 1):
        if line.lstrip().startswith("#"):
            continue
        listed = LISTED.match(line)
        if listed:
            _, _, file, url, digest = listed.groups()
            found.append(Download(script, file, url, "sha256", digest))
            continue
        assignment = ASSIGNMENT.match(line)
        if assignment:
            value = _expand(assignment.group(2), variables)
            if value is not None:
                variables[assignment.group(1)] = value
            continue
        fetch = FETCH.match(line)
        if fetch:
            url_text, path_text, tool, digest_text = fetch.groups()
            if url_text == "$url":  # the loop over a list, read above
                continue
            url = _expand(url_text, variables)
            digest = _expand(digest_text, variables)
            file = _expand(path_text.rsplit("/", 1)[-1], variables)
            if None in (url, digest, file) or not url.split():
                raise SystemExit(f"{script}:{number}: a fetch this cannot resolve: {line.strip()}")
            upstream, *mirrors = url.split()
            found.append(Download(script, file, upstream, tool.removesuffix("sum"), digest, tuple(mirrors)))
        elif re.match(r"^\s*fetch\s", line) and "$url" not in line:
            raise SystemExit(f"{script}:{number}: a fetch in a shape this does not read: {line.strip()}")
    return found


def app_script(name: str) -> pathlib.Path:
    return APPS / name / "build.sh"


def port_script(name: str) -> pathlib.Path:
    return PORTS / name / "build.sh"


def ports_linked(app: str) -> list[str]:
    """The ferrousli ports an app's script builds and links: `tools/ports/<port>/`."""
    text = app_script(app).read_text()
    return sorted(set(re.findall(r"tools/ports/([a-z0-9+-]+)/build\.sh", text)))


def cache_dirs() -> list[pathlib.Path]:
    """Where the build scripts keep what they fetched, as they name it."""
    home = pathlib.Path.home() / ".local" / "share" / "ferrix"
    ports = pathlib.Path(os.environ.get("FERRIX_PORTS", home / "ports" / "ferrousli"))
    busybox = pathlib.Path(os.environ.get("FERRIX_BUSYBOX", home / "busybox" / "ferrousli"))
    found = [ports / "src", busybox / "src"]
    # tools/common/fetch/fetch-shell-trees.sh's, where package-desktop.py ran it.
    if os.environ.get("FERRIX_SHELL_TREES"):
        found.append(pathlib.Path(os.environ["FERRIX_SHELL_TREES"]) / "src")
    return found


def verify(path: pathlib.Path, download: Download) -> bool:
    digest = hashlib.new(download.algorithm)
    with path.open("rb") as file:
        while chunk := file.read(1 << 20):
            digest.update(chunk)
    return digest.hexdigest() == download.digest


def local(download: Download, fetch_into: pathlib.Path | None = None) -> pathlib.Path:
    """The file, checked against its pin: from the build's own cache, or
    fetched into `fetch_into` when it is not there. A file whose checksum
    differs is refused, wherever it came from."""
    for directory in cache_dirs():
        path = directory / download.file
        if path.is_file():
            if not verify(path, download):
                raise SystemExit(f"{path}: does not match its pinned {download.algorithm} in {download.script}")
            return path
    if fetch_into is None:
        raise SystemExit(
            f"{download.file} is not in {', '.join(map(str, cache_dirs()))}: build first, "
            f"or let this fetch it ({download.url})"
        )
    fetch_into.mkdir(parents=True, exist_ok=True)
    path = fetch_into / download.file
    if path.is_file():
        if not verify(path, download):
            raise SystemExit(f"{path}: does not match its pinned {download.algorithm} in {download.script}")
        return path
    # With curl, as the build scripts' own `fetch` does: some of these
    # servers turn away other clients (busybox.net answers urllib 418). The
    # upstream first, then each mirror, until one gives the pinned bytes;
    # only a file that matches is moved into place.
    part = path.with_suffix(path.suffix + ".part")
    for url in (download.url, *download.mirrors):
        print(f"fetching {url}", flush=True)
        part.unlink(missing_ok=True)
        if subprocess.run(["curl", "-fsSL", "--max-time", "300", "-o", str(part), url]).returncode != 0:
            print(f"{url} did not answer; trying the next source", flush=True)
            continue
        if verify(part, download):
            part.rename(path)
            return path
        print(f"{url} gave a {download.file} that does not match its pinned {download.algorithm}", flush=True)
    part.unlink(missing_ok=True)
    raise SystemExit(f"no source gave {download.file} with its pinned {download.algorithm}: "
                     f"{' '.join((download.url, *download.mirrors))}")
