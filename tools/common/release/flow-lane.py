#!/usr/bin/env python3
"""Decide a pull request's lane and post it as the status `flow/lane`.

Run by .github/workflows/flow.yml on pull_request_target, with the base
branch checked out and the pull request's code never run (docs/REPO-FLOW.md,
"Two lanes"). It reads the event and asks the API; it trusts nothing a
contributor can choose -- not the branch name, not the title, not a label
the contributor set.

  owner lane    the head branch is in this repository (a fork never is) and
                the author has write, maintain or admin permission on it, or
                is this repository's own github-actions bot (the promotion
                pull requests). The lane is success at once, and auto-merge is
                switched on: the pull request merges itself when the required
                checks pass. No person clicks anything.
  outside lane  everyone else. Never auto-merged. The lane stays pending
                until both hold on the current head commit:
                  * an APPROVED review of that commit by someone with write
                    permission (CODEOWNERS names the owner on every path);
                  * the label `security-reviewed`, put on by someone with
                    write permission after the last push -- every new push
                    takes it off again.
                The person who approves then merges by hand.

Writes `lane=owner|outside` to $GITHUB_OUTPUT for the workflow's later steps.
"""
from __future__ import annotations

import json
import os
import subprocess
import sys

LABEL = "security-reviewed"
CONTEXT = "flow/lane"
WRITE = {"admin", "maintain", "write"}


def gh(*args: str, check: bool = True) -> str:
    p = subprocess.run(["gh", *args], capture_output=True, text=True)
    if check and p.returncode:
        sys.exit(f"gh {' '.join(args[:3])}: {p.stderr.strip()}")
    return p.stdout


def permission(repo: str, login: str) -> str:
    out = gh("api", f"repos/{repo}/collaborators/{login}/permission", check=False)
    try:
        return json.loads(out).get("permission", "none")
    except json.JSONDecodeError:
        return "none"


def status(repo: str, sha: str, state: str, desc: str) -> None:
    gh("api", "-X", "POST", f"repos/{repo}/statuses/{sha}", "-f", f"state={state}",
       "-f", f"context={CONTEXT}", "-f", f"description={desc[:140]}")


def main() -> int:
    repo = os.environ["GITHUB_REPOSITORY"]
    event = json.load(open(os.environ["GITHUB_EVENT_PATH"]))
    action = event.get("action", "")
    pr = event["pull_request"]
    num, sha = pr["number"], pr["head"]["sha"]
    author = pr["user"]["login"]
    same_repo = (pr["head"]["repo"] or {}).get("full_name") == repo

    owner = same_repo and (
        author == "github-actions[bot]" or permission(repo, author) in WRITE
    )
    lane = "owner" if owner else "outside"
    with open(os.environ.get("GITHUB_OUTPUT", os.devnull), "a") as out:
        out.write(f"lane={lane}\n")

    if owner:
        status(repo, sha, "success", f"owner lane: {author}")
        if not pr.get("draft") and action in ("opened", "reopened", "ready_for_review", "synchronize"):
            # Merge commit, so develop stays an ancestor of main (docs/REPO-FLOW.md).
            gh("pr", "merge", str(num), "--repo", repo, "--auto", "--merge", check=False)
        print(f"owner lane ({author})")
        return 0

    labels = {lb["name"] for lb in pr.get("labels", [])}
    if action == "synchronize" and LABEL in labels:
        gh("api", "-X", "DELETE", f"repos/{repo}/issues/{num}/labels/{LABEL}", check=False)
        labels.discard(LABEL)

    missing = []
    if LABEL in labels:
        # Who put the label on last, and do they have write permission?
        events = json.loads(gh("api", f"repos/{repo}/issues/{num}/events", "--paginate", "--slurp"))
        last = None
        for page in events:
            for e in page:
                if e.get("event") == "labeled" and (e.get("label") or {}).get("name") == LABEL:
                    last = e
        if not last or permission(repo, last["actor"]["login"]) not in WRITE:
            missing.append(f"`{LABEL}` not put on by a maintainer")
    else:
        missing.append(f"label `{LABEL}` (after reading the security review)")

    reviews = json.loads(gh("api", f"repos/{repo}/pulls/{num}/reviews", "--paginate", "--slurp"))
    approved = [r for page in reviews for r in page
                if r["state"] == "APPROVED" and r["commit_id"] == sha
                and permission(repo, r["user"]["login"]) in WRITE]
    if not approved:
        missing.append("a code owner's approval of this commit")

    if missing:
        status(repo, sha, "pending", "outside lane, waiting for: " + "; ".join(missing))
    else:
        status(repo, sha, "success", f"outside lane: approved by {approved[-1]['user']['login']} after security review")
    print(f"outside lane ({author}); missing: {missing or 'nothing'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
