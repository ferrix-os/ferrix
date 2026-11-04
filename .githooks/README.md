# Hooks

Version-controlled hooks, so the rule they enforce lives with the code rather
than in each clone's `.git/hooks`. Git does not pick them up on its own — one
line per clone, once:

```
git config core.hooksPath .githooks
```

`core.hooksPath` is local configuration and cannot be committed, which is the
one thing about this arrangement that has to be remembered rather than enforced.

It was not remembered. Both checks below are hooks, so both were dormant in a
clone where that line had never run: `.git/hooks` held nothing but Git's
samples, and eight commits carrying the trailer were written and seven pushed
without either hook executing once. Two checks behind one un-committable switch
are not two places — they are one control with two names, and it failed closed
for the rule and open for everything else.

So the rules are now checked in a third place that needs no arming: the **One
author per commit** job in `.github/workflows/ci.yml`, which runs
`tools/common/check/check-commit-authors.py` over the range a push or pull request adds.
The hooks below are still worth arming — they fail in a second, where the fix
is still a message edit rather than a rebase — but they are the convenience,
and CI is the guarantee.

`cargo xtask check` fails on its first gate when this clone is not armed, so
the missing line reports itself rather than waiting to be noticed.

## `commit-msg` — no `Co-authored-by`, no tool as author or committer

Ferrix commits name one author. A `Co-authored-by:` trailer is refused at the
moment it is written, which is where the error is cheap and the fix is to edit
the message.

The author and the committer are refused too when their name or address
matches `claude` or `anthropic`: a Ferrix commit is its owner's, and agents go on
committing under the owner's identity. To repair the tip, `git commit --amend
--reset-author` (a plain amend keeps the author, so the hook refuses it again);
for older commits, `git rebase --exec 'git commit --amend --no-edit
--reset-author' BASE`.

Two things are deliberately *not* matched, because both would be false
positives that teach people to reach for `--no-verify`:

* the phrase inside a sentence — only a trailer at the start of a line counts;
* anything below the scissors line, which is the diff `git commit -v` appends —
  including the diff of this directory, whenever a hook changes.

## `pre-push` — the same rules, over what is actually being published

`commit-msg` runs when a message is written. That misses a `--no-verify`, a
rebase that replays a message without reopening it, and any commit that arrived
from somewhere else. So the check is repeated over every commit in the range
being pushed, which is the last point at which a rewrite is still free.

For a new branch the range is everything not already reachable from a
remote-tracking ref, so pushing a branch does not re-examine — or re-reject —
history that is already public.
