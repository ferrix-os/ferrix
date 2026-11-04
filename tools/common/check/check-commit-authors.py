#!/usr/bin/env python3
"""Assert no commit carries a `Co-authored-by:` trailer, or names a tool as its
author or committer (customer, 2026-11-04: worse than a co-author).

Ferrix records one author per commit. `.githooks/commit-msg` refuses a trailer
when a message is written and `.githooks/pre-push` refuses it again over the
range being published, which the hooks' README describes as the rule being
"checked in two places rather than one".

It was not. Both places are hooks, and hooks only run when a clone has been
told where they live:

    git config core.hooksPath .githooks

That line is local configuration, it cannot be committed, and in this clone it
was never run. So neither hook had ever executed, `.git/hooks` held nothing but
Git's samples, and eight commits carrying the trailer were written and seven of
them pushed without a single check firing. Two places behind one un-committable
switch is one control with two names, not defence in depth.

This is the third place, and the only one that does not depend on anybody
remembering anything: CI runs it on the range a push or a pull request adds.

Two things are deliberately *not* matched, so the gate never teaches anyone to
reach for `--no-verify`:

  * the phrase inside a sentence -- only a trailer at the start of a line
    counts, which is why the commit that introduced the hooks may describe the
    rule in prose and still pass;
  * history that is already public -- the check runs over a range, never over
    the whole tree, so it reports what a change is adding and not what it
    inherited.

Usage:
    python3 tools/common/check/check-commit-authors.py                  # unpushed commits
    python3 tools/common/check/check-commit-authors.py BASE HEAD        # an explicit range
    python3 tools/common/check/check-commit-authors.py --hooks          # is this clone armed?
"""

from __future__ import annotations

import argparse
import pathlib
import re
import subprocess
import sys

# A trailer, at the start of a line, in any of the spellings Git and the tools
# that write them use. `git interpret-trailers` is case-insensitive here, so
# this is too.
TRAILER = re.compile(r"^[ \t]*co-authored-by[ \t]*:", re.IGNORECASE | re.MULTILINE)

# A tool as the author or the committer, in the name or the address. Twelve
# commits of 2026-11-02 came from a session whose identity was
# `Claude <noreply@anthropic.com>`; no trailer check could see them.
TOOL_IDENTITY = re.compile(r"claude|anthropic", re.IGNORECASE)

# The all-zero object id GitHub sends for `github.event.before` when a branch is
# new or has been force-pushed, and Git sends for a deletion.
EMPTY = "0" * 40

HOOKS_PATH = ".githooks"

#: The hooks this gate exists to see armed.
REQUIRED_HOOKS = ("commit-msg", "pre-push")


def git(*arguments: str) -> str:
    """Run a git command and return its stdout, stripped."""
    result = subprocess.run(
        ["git", *arguments],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise RuntimeError(f"git {' '.join(arguments)}: {result.stderr.strip()}")
    return result.stdout.strip()


def hooks_are_armed() -> bool:
    """Whether this clone has been pointed at the version-controlled hooks.

    Tested by asking whether the configured directory *holds the hooks*, not by
    matching the string it was written as. `git config core.hooksPath` accepts
    a relative path and an absolute one alike and both arm the hooks equally;
    and in a linked worktree the natural absolute setting points at the main
    checkout's copy, which is the same tracked files at a different path. A
    gate that compared spellings called a correctly armed clone unarmed, and a
    gate that cries wolf is one people learn to pass with `--no-verify`.
    """
    try:
        configured = git("config", "--get", "core.hooksPath")
    except RuntimeError:
        # `--get` exits 1 when the key is unset, which is exactly the state this
        # gate exists to report.
        return False
    if not configured:
        return False

    # A relative path is resolved against the working tree's root, which is the
    # directory every hook git runs starts in.
    directory = pathlib.Path(configured)
    if not directory.is_absolute():
        try:
            directory = pathlib.Path(git("rev-parse", "--show-toplevel")) / directory
        except RuntimeError:
            return False

    return all((directory / hook).is_file() for hook in REQUIRED_HOOKS)


def commits_in(base: str | None, head: str) -> list[str]:
    """The commits a range adds, newest first.

    With no base -- or the all-zero one GitHub sends for a new branch -- the
    range is everything not already reachable from a remote-tracking ref, which
    is the same range `pre-push` inspects: what this change would publish, and
    not what was published before it.
    """
    if base and base != EMPTY:
        revisions = git("rev-list", f"{base}..{head}")
    else:
        revisions = git("rev-list", head, "--not", "--remotes")
    return revisions.split() if revisions else []


def offending_lines(commit: str) -> list[str]:
    """Every trailer line in one commit's message, as `line-number: text`."""
    message = git("log", "-1", "--format=%B", commit)
    return [
        f"{number}: {line.strip()}"
        for number, line in enumerate(message.splitlines(), start=1)
        if TRAILER.match(line)
    ]


def tool_identities(commit: str) -> list[str]:
    """The author or committer of one commit, where either is a tool."""
    author = git("log", "-1", "--format=author %an <%ae>", commit)
    committer = git("log", "-1", "--format=committer %cn <%ce>", commit)
    return [ident for ident in (author, committer) if TOOL_IDENTITY.search(ident)]


def report_unarmed() -> None:
    """Explain the one step that turns the hooks from files into checks."""
    print("check-commit-authors: this clone's hooks are not armed.", file=sys.stderr)
    print(file=sys.stderr)
    print(f"    git config core.hooksPath {HOOKS_PATH}", file=sys.stderr)
    print(file=sys.stderr)
    print(
        "Until that runs, `.githooks/commit-msg` and `.githooks/pre-push` are\n"
        "files rather than checks, and nothing local refuses a trailer. This is\n"
        "how eight of them were written and seven pushed.",
        file=sys.stderr,
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    _ = parser.add_argument(
        "--hooks",
        action="store_true",
        help="check that core.hooksPath is armed, and nothing else",
    )
    _ = parser.add_argument("base", nargs="?", help="the commit the range starts after")
    _ = parser.add_argument("head", nargs="?", default="HEAD", help="the range's tip")
    args = parser.parse_args()

    if args.hooks:
        if hooks_are_armed():
            print(f"commit-authors: hooks armed ({HOOKS_PATH})")
            return 0
        report_unarmed()
        return 1

    try:
        commits = commits_in(args.base, args.head)
    except RuntimeError as error:
        print(f"check-commit-authors: {error}", file=sys.stderr)
        return 1

    offenders = [
        (commit, lines)
        for commit in commits
        if (lines := offending_lines(commit) + tool_identities(commit))
    ]

    if offenders:
        print(
            "check-commit-authors: refusing commits by a tool, or with a Co-authored-by trailer.",
            file=sys.stderr,
        )
        print(file=sys.stderr)
        for commit, lines in offenders:
            print(f"  {git('log', '-1', '--format=%h %s', commit)}", file=sys.stderr)
            for line in lines:
                print(f"      {line}", file=sys.stderr)
        print(file=sys.stderr)
        print(
            "Ferrix commits name one author, never a tool. Rewrite them -- `git rebase -i`, or\n"
            "`git commit --amend --reset-author` for the tip (a plain amend keeps the author), "
            "or for older commits\n"
            "`git rebase --exec 'git commit --amend --no-edit --reset-author' BASE` -- and push again.",
            file=sys.stderr,
        )
        return 1

    print(f"commit-authors: {len(commits)} commit(s) in range, none co-authored, none by a tool")
    return 0


if __name__ == "__main__":
    sys.exit(main())
