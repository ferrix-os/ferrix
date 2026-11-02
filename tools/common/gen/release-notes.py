#!/usr/bin/env python3
"""Print a tag's release notes, or its release title, from docs/RELEASES.md.

    python3 tools/common/gen/release-notes.py <tag>           # the notes, as Markdown
    python3 tools/common/gen/release-notes.py --title <tag>   # "<tag> (<date>)"

A tag's section is the one whose heading starts `## <tag> (`, as in
`## v0.1.1 (2026-10-21), features at 336e03bca`; the older milestones' form,
`## <tag> — `, is still read. It runs to the next `## ` heading. The release
workflow publishes exactly this, so a tag without a section fails here
rather than going out as an empty release.

A release candidate, `<tag>-rc<n>`, is read from its own section when it has
one, and otherwise from `<tag>`'s, under a line that says what it is: the
build the release is cut from, published as a pre-release through the
release's own path before the release itself.
"""

import re

import sys
from pathlib import Path

RELEASES = Path(__file__).resolve().parents[3] / "docs" / "RELEASES.md"
REPO = "https://github.com/ferrix-os/ferrix"


CANDIDATE = re.compile(r"^(v\d+(?:\.\d+)*)-rc(\d+)$")


def section(tag):
    try:
        return own_section(tag)
    except SystemExit:
        candidate = CANDIDATE.match(tag)
        if not candidate:
            raise
        release, number = candidate.groups()
        heading, body = own_section(release)
        heading = heading.replace(f"## {release}", f"## {tag}", 1)
        lead = [
            "",
            f"**Release candidate {number} of {release}**, published as a pre-release: the "
            f"build {release} is cut from, put through the release's own path first. "
            f"What follows is {release}'s notes; the downloads here are named for {tag}.",
        ]
        return heading, lead + body


def own_section(tag):
    lines = RELEASES.read_text(encoding="utf-8").splitlines()
    start = None
    for i, line in enumerate(lines):
        if line.startswith((f"## {tag} (", f"## {tag} — ")) or line == f"## {tag}":
            start = i
            break
    if start is None:
        sys.exit(f"release-notes: docs/RELEASES.md has no section for {tag}")
    end = next((j for j in range(start + 1, len(lines)) if lines[j].startswith("## ")), len(lines))
    return lines[start], lines[start + 1:end]


def main():
    args = sys.argv[1:]
    if args[:1] == ["--title"]:
        heading, _ = section(args[1])
        # "## v0.1.1 (2026-10-21), features at 336e03bca" -> "v0.1.1 (2026-10-21)"
        print(heading[3:].split(",")[0])
        return
    tag = args[0]
    _, body = section(tag)
    text = "\n".join(body).strip()
    print(text)
    print()
    print("---")
    print(f"Boot it: `git checkout {tag} && cargo xtask run --arch x86_64`. "
          f"Everything else is in the [README]({REPO}/blob/{tag}/README.md) "
          f"and on the [website](https://ferrix-os.org/).")
    print()
    print("The desktop's wallpaper is attached at 1920x1080, 2560x1440 and 3840x2160 "
          "(`ferrix-wallpaper-<size>.png`), for your own desktop too.")


if __name__ == "__main__":
    main()
