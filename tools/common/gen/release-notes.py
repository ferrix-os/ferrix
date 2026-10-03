#!/usr/bin/env python3
"""Print a tag's release notes, or its release title, from docs/RELEASES.md.

    python3 tools/common/gen/release-notes.py <tag>           # the notes, as Markdown
    python3 tools/common/gen/release-notes.py --title <tag>   # "<tag> — <date>"

A tag's section is the one whose heading starts `## <tag> — `. It runs to the
next `## ` heading. The release workflow publishes exactly this, so a tag
without a section fails here rather than going out as an empty release.
"""

import sys
from pathlib import Path

RELEASES = Path(__file__).resolve().parents[3] / "docs" / "RELEASES.md"
REPO = "https://github.com/ferrix-os/ferrix"


def section(tag):
    lines = RELEASES.read_text().splitlines()
    start = None
    for i, line in enumerate(lines):
        if line.startswith(f"## {tag} — ") or line == f"## {tag}":
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
        # "## stage-9 — 2026-09-13, commit 80d0ea7" -> "stage-9 — 2026-09-13"
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
          f"and on the [website](https://ferrix-os.github.io/).")


if __name__ == "__main__":
    main()
