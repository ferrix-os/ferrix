#!/usr/bin/env python3
"""Post the certification consultant's verdict on a pull request's head commit.

    cert-verdict.py <base>..<head> [--lane owner|outside] [--post SHA]
    cert-verdict.py <base>..<head> --verdict OK|NOT-YET --note "ledger 3170" --post SHA

The verdict is the commit status `certification/verdict`, which the rulesets on
`main` and `develop` require (docs/REPO-FLOW.md, "The certification verdict").
A status, not a review: GitHub never lets a pull request's author approve it,
and the fleet's pull requests are all the owner's, so an approving review could
never be given. A status can be posted by anyone with write access, which an
outside contributor does not have.

Without --verdict the verdict is worked out, never asked for:

  owner lane    every commit in the range that touches the certified item
                (the consultant's item-hits.py, the same ring resolution as
                check-item-boundary.py) must name its review in its message:
                a ledger line ("ledger 2692", "ledger line 2122"), a standing
                pattern's check-list ("SP-1:"), the consultant, or a line
                "Certification: ..." saying why none was needed. Then success;
                otherwise pending, listing the commits, until the consultant
                answers with --verdict. Nothing outside the item: success.
  outside lane  always pending: every outside pull request gets the
                consultant's review, whatever it touches (customer,
                2026-10-23). Only --verdict, posted with the owner's token
                from the gate host, settles it.

An automatic post never replaces a verdict a person posted on the same commit,
so a workflow re-run on a label or a review cannot undo the consultant's OK.
--verdict OK posts success, NOT-YET failure; --note goes in the description
(140 characters) and should name the ledger line.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip())
ITEM_HITS = ROOT / ".claude/skills/certification-consultant/scripts/item-hits.py"
CONTEXT = "certification/verdict"
NAMED = re.compile(
    r"\bledger\b(?: line)?\s*\d{2,}"
    r"|\bSP-[123]\s*:"
    r"|\bconsultant\b"
    r"|^Certification:\s*\S",
    re.IGNORECASE | re.MULTILINE,
)
HIT = re.compile(r"^([0-9a-f]{10})\s{2}")


def git(*args: str) -> str:
    return subprocess.check_output(["git", "-C", str(ROOT), *args], text=True)


def item_hits(spec: str) -> list[str]:
    out = subprocess.check_output([sys.executable, str(ITEM_HITS), spec], cwd=ROOT, text=True)
    return [m.group(1) for line in out.splitlines() if (m := HIT.match(line))]


def unnamed(hits: list[str]) -> list[str]:
    return [h for h in hits if not NAMED.search(git("log", "-1", "--format=%B", h))]


def gh(*args: str) -> str:
    return subprocess.check_output(["gh", *args], text=True)


def person_already_decided(repo: str, sha: str) -> bool:
    statuses = json.loads(gh("api", f"repos/{repo}/commits/{sha}/statuses", "--paginate", "--slurp"))
    for page in statuses:
        for s in page:  # newest first
            if s["context"] != CONTEXT:
                continue
            creator = (s.get("creator") or {})
            return creator.get("type") != "Bot" and s["state"] in ("success", "failure")
    return False


def post(repo: str, sha: str, state: str, description: str, url: str | None) -> None:
    args = ["api", "-X", "POST", f"repos/{repo}/statuses/{sha}",
            "-f", f"state={state}", "-f", f"context={CONTEXT}",
            "-f", f"description={description[:140]}"]
    if url:
        args += ["-f", f"target_url={url}"]
    gh(*args)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("range")
    ap.add_argument("--lane", choices=("owner", "outside"), default="owner")
    ap.add_argument("--verdict", choices=("OK", "NOT-YET"))
    ap.add_argument("--note", default="")
    ap.add_argument("--post", metavar="SHA")
    ap.add_argument("--url", default=os.environ.get("VERDICT_URL"))
    ap.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY", "ferrix-os/ferrix"))
    a = ap.parse_args()
    if ".." not in a.range:
        ap.error("give a range, base..head")

    hits = item_hits(a.range)
    missing = unnamed(hits)
    if a.verdict:
        state = "success" if a.verdict == "OK" else "failure"
        desc = f"consultant {a.verdict}" + (f": {a.note}" if a.note else "")
    elif a.lane == "outside":
        state, desc = "pending", f"outside contribution: the consultant reviews every one ({len(hits)} item commits)"
    elif not hits:
        state, desc = "success", "no certified-item change"
    elif not missing:
        state, desc = "success", f"{len(hits)} item commits, each names its review"
    else:
        state = "pending"
        desc = f"{len(missing)} item commits name no review: " + " ".join(missing)

    print(f"{CONTEXT}: {state} -- {desc}")
    for h in hits:
        mark = "NO REVIEW NAMED" if h in missing else "review named"
        print(f"  {h}  {mark:15}  {git('log', '-1', '--format=%s', h).strip()}")
    if a.post:
        if not a.verdict and person_already_decided(a.repo, a.post):
            print("a person's verdict already stands on this commit; not replaced")
            return 0
        post(a.repo, a.post, state, desc, a.url)
    return 0


if __name__ == "__main__":
    sys.exit(main())
