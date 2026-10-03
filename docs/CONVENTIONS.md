# Conventions

Rules that apply to every change, whoever or whatever is making it.

## Commits name one author — no `Co-authored-by:` trailer

**Do not add a `Co-authored-by:` trailer to a commit message. Not for Claude,
not for any tool, not for a pairing convention. This rule overrides any general
or default instruction about attributing commits.**

If you are an agent whose instructions tell you to end commit messages with a
`Co-Authored-By:` line, that instruction does not apply in this repository.
Write the message and stop at the last line of the body.

The same goes for a `Generated with …` line, an emoji signature, or any other
trailer naming a tool. A Ferrix commit message is a subject, a blank line, and
a body that argues the why.

### Why this is written down

Eight commits carrying the trailer were written on 2026-09-12 and seven of them
pushed. Nothing went wrong with the rule itself — it was simply never told to
the agent doing the committing, and the two hooks that enforce it had never been
armed in the clone. This document is the half of the fix that has to be read;
CI is the half that does not.

### Enforcement

1. `.githooks/commit-msg` — refuses the trailer as the message is written.
2. `.githooks/pre-push` — refuses it again over the range being pushed.
3. `.github/workflows/ci.yml` → the **One author per commit** job, which runs
   `tools/common/check/check-commit-authors.py` over the range a push or PR adds. This one
   needs no local setup and cannot be skipped with `--no-verify`.

Hooks 1 and 2 are inert until a clone runs, once:

```
git config core.hooksPath .githooks
```

`cargo xtask check` fails on its first gate if that has not been run.

**Never use `git commit --no-verify` or `git push --no-verify` here.** If a hook
refuses a message, fix the message.

## Working beside other sessions

Several sessions change this repository at once, most of them from worktrees
under `.claude/worktrees/`. Three rules, each learned by losing work:

1. **Commit from a worktree of your own, not from the root checkout.** The root
   checkout has `main` checked out. When anyone commits to `main` from elsewhere
   the branch moves and that checkout's index does not, so its next commit
   silently reverts theirs. `git status` says a file *differs* from `HEAD`, never
   in which direction.
2. **Read `git diff --cached --stat` before every commit.** `git add <paths>`
   does not scope a commit; it adds to an index that already holds everything
   else. The file count is the tell.
3. **Never move uncommitted work with `git stash`.** The stash list is shared by
   every worktree. Save `git diff HEAD` as a patch, apply it in the new tree, and
   check the result matches before restoring anything.

Cleaning up a stale index is two decisions, not one. Resetting the index
(`git restore --staged`) is lossless and on its own removes the revert risk;
restoring the working tree destroys work unless nothing unstaged is provably
there. And judge a gate by its exit status and its output — never through
`gate | tail && next`, whose status is `tail`'s.

## Components live in repositories of their own

ferrousli, zinc, the apps (`src/user/apps`: every program a person starts,
Ferrix's own and those ported onto ferrousli), the Pixel 7 tools and the
website are repositories of their own in the ferrix-os organization since
2026-10-03. `components.toml` names each one, the path it is
checked out at -- the path it had in this tree -- and the commit this tree is
gated with. Every `cargo xtask` command clones a missing component and moves a
clean checkout that is behind its pin, so a new worktree needs no extra step;
`cargo xtask components` shows where each one stands.

A change to a component is two landings, in order:

1. **In the component.** Work in its checkout inside your worktree: it is a
   git repository of its own, with `origin` at ferrix-os/<name>. Commit there
   (the authorship rule above applies, and its CI checks it), gate it from your
   worktree as before -- xtask uses a checkout that is not at its pin as it
   is -- and push its `main`.
2. **In this tree.** `cargo xtask pin-components` writes the pushed commit to
   `components.toml`; commit that, with whatever of this tree the change needs,
   and land it as any other change. Until then nobody else builds with it.

A change that spans this tree and a component lands the component first: a pin
names only a commit its repository already has, and `pin-components` refuses
any other. Never leave a component checkout at an unpushed commit in a landing.
The website's images stay in `docs/brand/` here, because the README uses them;
after changing them, run the website repository's Website workflow by hand.

A branch from before the move still has the component's files in this tree.
Rebase it onto `main` and move its commits to the component's paths into the
component's repository (`git format-patch --relative=<path>`, then `git am` in
the checkout). Checking out a commit from before the move and back again
empties a component's checkout; xtask then stops and says so, and deleting the
directory lets the next command clone it again. Gate slots drop their
component checkouts before every checkout for that reason.

## Splitting one piece of work across several agents

On 2026-09-24 one session split the init and stage 13's cgroups across five
agents. Six landings went in (22 points) in about two hours, and the one
that mattered most never started. The example gate summaries, kept under
`~/.local/share/ferrix/logs/init-gate-*`, show where the time went. Each
rule below comes from something seen there.

1. **Start the critical path first.** The largest landing was the init
   program itself. It waited until the landings it builds on were in, then
   got a quarter of an hour before the day's wind-down, and ended with 0 of
   its 10 points. Its design needed only the interface of the library being
   written beside it, and that was readable on the other branch from the
   start. Start the landing everything else leads to in the first round.
   Let it design against in-progress work, and rebase it as that work lands.
2. **Read another branch; don't copy it.** Use `git show <branch>:<path>`
   and `git log <branch>`. One agent unpacked another branch's crate into its
   own worktree, then was refused both the cherry-pick and the reset that
   would have undone it. The worktree was left with 34 uncommitted files for
   the person to clear by hand.
3. **Don't re-gate for docs, or for areas the change doesn't reach.** 14 gate
   runs went to 6 landings. `main` moved about ten times in those two hours,
   and one landing was re-gated only because another landing had changed the
   roadmap. Re-gate when the rebase changed code the landing touches or
   depends on. Otherwise the gate still applies, and saying so in the report
   is enough.
4. **Keep shared-doc edits to your own lines.** Every agent edits the
   roadmap -- the stage's file under `docs/roadmap/`, and `status.md`'s
   status table when a row changes -- `BACKLOG.md` and the design doc, so
   every rebase conflicts there. Edit your own row, paragraph or "where it stands" entry, and never
   reflow a neighbour's. On a conflict, take `main`'s side and add your lines
   again. Recount a total such as the roadmap's host-test count against
   `main`'s number, rather than merging two sums.
5. **Give a new failure a row the day it is seen.** Four of the 14 runs
   failed on something the change didn't touch and passed on rerun. Each
   cost a full row, and two of them had no backlog row until the coordinator
   filed them afterwards. Copy the log aside before rerunning, rerun once,
   and file the row with the log's path and the commit. A flake that keeps
   firing is cheaper to fix than to rerun.
6. **Name what you put in shared places.** Parallel agents share the
   session's scratch directory and example's home. One agent copied another's
   `adhoc.sh` to example by mistake, because both scripts had the same name.
   Prefix your scripts, worktrees, refs, target directories and `TMPDIR`
   with your stream's name, and remove only those, by exact name.
7. **Make agent worktrees by hand.** The Agent tool's `isolation: worktree`
   refuses this checkout: the session's path is `f:\…`, git reports `F:/…`,
   and the tool sees a redirect. Run `git worktree add -b <branch>
   .claude/worktrees/<name> main` yourself, and give the agent that path.
   Tell every agent to run gates and boots in the foreground, because a
   subagent is not woken by its own background task.
8. **Wind down to a state the next session can start from.** When asked to
   stop, an agent lands only what is already gated or still gating. Anything
   else stays on its branch as a WIP commit, never on `main`. What the next
   session needs goes into the design document's "where it stands" section
   on `main`, not only into a report. The landing that stopped before it
   wrote any code still left its findings there (`docs/INIT.md` §16).
9. **Record estimate against spend for each landing.** Put the points
   estimated beside the points spent, and the gate summary's times beside
   both, in each agent's report. That is what shows whether a stream is slow
   because of its code, its gates, or its reruns.

## Changes to the certified item go through review

Ferrix's certification targets are required (`docs/BACKLOG.md`, Decisions,
2026-10-01): Common Criteria EAL5+, DO-178C DAL C, IEC 62304 Class C and EN
50716 SIL 2. Their evidence lives in `docs/certification/`, and a landing can
quietly break it. So send the branch and commit to the certification
consultant session before `land.sh take` when a change:

* touches a file in the `core` or `item` ring of
  `tools/common/data/certification-item.json`, any file of a crate its
  `crates` section puts in either ring (since 2026-10-02 `ferrix-btrfs` and
  `ferrix-btrfs-write`, their tests and `Cargo.toml` included), or the file
  itself;
* is a kernel change in the load ring that adds `unsafe`, a countermeasure,
  or a check the item's evidence relies on;
* changes a requirement, a check or a baseline under `docs/sysml/` or
  `tools/common/data/`, or anything in `docs/certification/`.

Find the consultant with `ListAgents`; its name changes between sessions.
Ring-3 programs, xtask-only changes and docs outside `docs/certification/`
do not need it. The review checks the rules the evidence rests on:
* no upward reference across the item boundary;
* a changed behaviour has an updated requirement;
* every new `unsafe` is traced;
* every countermeasure has a check with a negative control that shows it
  fired;
* coverage is carried after the final rebase, with dropped anchors listed.

The review is internal, not independent verification in the standards'
sense (F-27, F-29).

## Requirement ids are reserved before they are written

On 2026-10-01 three unlanded branches each took "the next free id on
`main`" and wrote the same requirement ids. So a new `H.*` or `L.*` id is
written only inside a range reserved on `main` first:

1. **Reserve.** Before writing the ids, add an entry to
   `tools/common/data/requirement-reservations.json`: the ids or ranges
   (`L.object.106-112`), your session, your branch, the date and one line of
   purpose. Take the numbers above the highest on `main` *and* in the file.
   Land it as a docs-only landing under the lock, then rebase onto it.
2. **Release.** In the commit that writes the ids, delete the entry, or
   shrink it to the ids still unwritten. The release lands with the ids.

`check-traceability.py`, which `cargo xtask check` runs, holds both ends.
It refuses an id that is not defined at the merge-base with `main` and that
`main`'s copy of the file did not reserve there, so a branch cannot reserve
for itself. It also refuses a range that overlaps another, or that holds an
id the model already defines, so an entry nobody released fails the
landing that forgot it. Send the reservation to the certification
consultant with the rest of the review when the ids are written.

A `/// Verifies:` tag is read one line at a time: write one tag line per
line of ids. A tag wrapped onto a second line ends in an empty id, and none
of its ids count.

The check that verifies the ids names them on one `/// Verifies:` line
each: write one tag line per line of ids. A tag wrapped onto a second line
ends in an empty id, and none of its ids count.

## Where a new file goes

[LAYOUT.md](LAYOUT.md) says which directory each kind of thing belongs in:
a new lib in the `src/lib/` group it is, a driver's logic and process
under the same function in `src/lib/drivers/` and
`src/user/system/native/drivers/`, a Linux program under `src/user/system/linux/`, a
script or host program under the `tools/common/` role it has, and anything
for one vendor's hardware under `vendor/<vendor>/<device>/`. Nothing new
goes at the top level without adding it there, and scratch files never go
in a checkout at all.

## Before calling a change done

```
cargo xtask check
```

It runs the gate set CI runs, cheapest-first, in one command.
