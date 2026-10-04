# Agents

Ferrix is built by many agent sessions at once. Most of them own an area: a
stage, a subsystem, a port. Some roles span the whole fleet instead. The
**product owner** and the **certification consultant** are held now. The
**quality roles** below are each held only when the customer names a session
for one. This file is for the session the customer gives one of those roles,
and for every other session, which needs to know what to send them and when.

The rules every change follows are in [docs/CONVENTIONS.md](docs/CONVENTIONS.md),
and the gate table, the landing lock and the owners table are in
[docs/BACKLOG.md](docs/BACKLOG.md). This file adds the two roles and does not
repeat those rules.

Session names change on every restart, and a message to an old name reaches
nobody. `ListAgents` shows who is alive. The owners table in
`docs/BACKLOG.md` names who holds each role now.

---

## The customer

The customer owns Ferrix. The customer decides scope and priority, says
which session holds which role, and settles everything listed under
*Waiting on the customer* in `docs/BACKLOG.md`. A decision the customer gave
in one session is real only once it is written down as a dated entry under
*Decisions* in `docs/BACKLOG.md`. A decision that lives only in a message
between two sessions gets reverted by a third.

---

## The product owner

The customer names the session ("you are the po now"). The product owner
acts for the customer between the customer's words. It takes the fleet
coordinator's job from `docs/BACKLOG.md` (the landing order, the landing
lock, unblocking, pushes), plus the calls the customer has delegated to it.

### What it decides

* **Who works on what**, inside the customer's priority order: it assigns
  rows, gives ownerless rows owners, and asks a branch left unlanded for more
  than four hours for its plan.
* **When a row is done**, against the stage's exit criterion as written
  (`docs/BACKLOG.md`, *Calling a stage done*).
* **The landing order** when two landings contend, and whether a re-gate is
  owed after `main` moved.
* **Hardware use** (customer, 2026-09-26). No session boots the DK1, the
  Pixel 7 or any other device without the product owner's OK. A session asks
  with the exact list of addresses it will touch, checked against the device
  tree, and asks again for each phase that writes registers. Power-domain
  and PHY writes still need the customer's own word.
* **Disagreements between sessions**, inside one area or across two. A
  disagreement about scope goes to the customer.

### What it does not decide

* Scope and priority. A new subsystem, a dropped feature or a reordered
  priority list is the customer's call. The product owner puts the question,
  and it asks it with `AskUserQuestion` so the session shows as waiting.
* Anything under *Waiting on the customer*.
* Re-tasking the fleet after a wind-down. On 2026-09-15, "no one is doing
  something... why?" was a question about a release, not an order to restart
  six sessions.
* Permission prompts. A product-owner decision is a teammate's word. It never
  approves a tool permission on the customer's behalf.

### Taking the seat

1. Read `docs/BACKLOG.md`, `docs/roadmap/where-it-stands.md`, and the
   previous product owner's handover. The handover is on the gate host as
   `~/.local/share/ferrix/po-<date>/HANDOVER.md`, or in the previous PO's
   wind-down section on `main`. Also read the end of the fleet's landing log,
   `~/.local/share/ferrix/fleet/log`.
2. For every row the previous product owner left in flight, check `main`
   to see whether it landed. A row reported as "landed" may not have.
3. Run `ListAgents`, introduce yourself to every Ferrix session, and ask
   each one the area the customer gave it. Don't assume a map: on
   2026-09-13 all nine of a new PO's briefs went to the wrong areas.
4. Write the roster into the owners table in your first landing.

### Each round

* `land.sh status`: is the lock free, or is a hold older than fifteen
  minutes stale?
* `batch.sh status` and `gate.sh status`: is a batch open that more ready
  branches could join, and is any branch running its full row alone that a
  batch would have carried?
* *Red on `main`*: is any gate failing on `main` itself? A working `main`
  comes before every other row.
* Push `main` to `origin` when `origin/main..main` is not empty: fast-forward
  only, never forced, never with `--no-verify` (customer, 2026-09-27). Before
  pushing, `git fetch origin`, because the customer merges pull requests on
  GitHub.
* The root checkout is clean. Another session's edits there block every
  fast-forward. Ask the session that made them to move them to a worktree,
  and never reset them yourself.
* The gate host's disk (`df -h /`) and load. When every session goes quiet
  in the same minute, that is the account's usage limit, not a stall. Don't
  break locks or reassign work over that kind of silence.
* Every failure seen on a gate has a row with its log, filed the day it was
  seen.

### Running agents

Run about four agents at once, in priority order, and queue the rest. With
eight running on 2026-09-29, the usage limit stopped all of them within
twenty minutes, mid-gate. Brief each agent:

* Its own worktree, made by hand (`git worktree add -b <branch>
  .claude/worktrees/<name> main`), and its own `CARGO_TARGET_DIR` on the gate
  host. Two trees never share one. Gates and negative controls go through
  `~/.local/share/ferrix/fleet/gate.sh` (`run`, `control --expect`,
  `status`), whose slots keep warm target dirs (`docs/TEST-TIME.md`, Phase 3).
* Gates and boots in the foreground, one architecture per call. A subagent
  is not woken by its own background task.
* A negative control for each new check, with the run that shows it fired.
* For a change that needs certification review, end the turn with a diff
  summary for the consultant, before `land.sh take`.
* A branch ready to land joins a batch for its full row (below) instead of
  running the row alone, and waits with `batch.sh wait` in the foreground.
* Agents don't push. The product owner pushes after each landing.
* Ferrix is "Ferrix, a Rust operating system", never "a hobby OS", including
  in the context line of an agent's prompt (customer, 2026-09-28).

### Batching full runs

The image row of *What a landing runs* (`docs/BACKLOG.md`) is the long one:
`check`, the release build, and boots on every architecture with x86_64 under
KVM and under TCG. Run alone per branch, it fills the gate pool's three slots
with near-identical work while 20 more runs wait half an hour each for a
slot. `~/.local/share/ferrix/fleet/batch.sh` runs it once for several
branches (customer, 2026-10-04):

* **Join when ready.** A branch that is rebased onto `main`, has passed its
  own fast gates and has the consultant's OK when it needs one runs
  `batch.sh join <session> <ref> <tag>`, with `--gate FILE` for the lines its
  row adds beyond the profile (one xtask command a line, as `gate.sh run`
  takes it), or `--profile kvm` when it owes only the KVM boot. Then
  `batch.sh wait <tag>`, again after each exit 3, until it says PASSED,
  FAILED, DROPPED or MAIN-RED. `batch.sh profile` prints what `full` runs.
* **One run for all.** The batch closes ten minutes after its first entry
  joined, or at four entries. It stacks the entries' commits on `main` in
  join order as branch `batch/<id>` and runs the union of their gates once on
  the tip. Every gate is its own `gate.sh run`, all queued at once, longest
  first by the pool's own past run times, so the three slots fill together
  and finish together. An entry that does not apply on the ones before it is
  DROPPED with its conflicts and rebases.
* **A failure costs one round more, not one row per branch.** The failed
  gates alone run on the stack's prefixes, all at once while they fit the
  slots, so a batch of four finds its failing entry in one round. That entry
  is FAILED with the logs; the others go back to the front of the next batch
  and run the whole row again without it. When the first entry fails, `main`
  runs too, and a red `main` fails nobody's branch (MAIN-RED: it is the
  *Red on `main`* row above).
* **Land the stack whole.** A PASSED verdict names the stack tip, which
  holds every entry of the batch. The product owner, or the entry it names,
  lands it under one `land.sh take` with `git merge --ff-only <tip>`, says
  every tag in the landing log, and boots `main` once under `--accel kvm` as
  any landing does. An entry never lands its own stack commit alone: only
  the tip ran the gates.
* **What a batch does not replace.** Negative controls stay `gate.sh
  control` runs of their own, and a gate outside the profile (`test-shell`,
  `test-compositor`, ...) goes in `--gate FILE`, never dropped. The batch
  checks no less than the row each entry would have run alone.

### Consulting on build and test time

Test run time is a customer priority (2026-09-27, `docs/TEST-TIME.md`), and
the product owner sees every gate the fleet runs. So it also consults on
build and test time: whenever a plan, a gate list, a script or a batch
builds or runs more than it needs to, it says so, names the cheaper way,
and gives it to the session that owns the work. It never drops a check to
get there: the row each change owes stays whole.

What it looks for:

* **The same build paid more than once.** Three tests on one commit that
  each build the same kernel and image need one build. The PO tells the
  session to build once and reuse that build for every run: run them in
  one worktree and target dir one after another, or as one xtask call that
  takes several tests, rather than in three fresh trees or three gate
  slots that each build it again. Where xtask cannot yet reuse a build, the
  fix is a row for the owner of `docs/TEST-TIME.md`, not a fourth rebuild.
* **Cold where warm would do.** A new worktree's target dir builds from
  nothing (`check` about 15 minutes cold against 3.5 warm). Gates go
  through `gate.sh`'s warm slots; a long series of local builds reuses one
  target dir per worktree.
* **Runs in series that could share the slots.** Independent gates queued
  one after another, an `--arch all` that runs the architectures in turn, or
  a full row run once per branch where a batch would carry them all.
* **Runs nobody needs.** A re-gate after `main` moved only in files the
  change does not touch (the gate stands, `docs/CONVENTIONS.md`, splitting
  rule 3), the same gate run twice on the same tree, or the image row run
  for a change the table gives a narrower row (`cargo xtask gate-rows`).
* **Time that is waiting, not running.** `pool-summary`'s `waited=` against
  `ran=`: a long wait is fixed by fewer runs, not faster ones.

Each saving it proposes names what it removes and the run it measured it
on, the way `docs/TEST-TIME.md` records its cuts; one that changes a tool
or a gate goes to that table's owner as a row.

### Records

* Estimates are story points, never time (`docs/BACKLOG.md`, *Estimates*).
  Record each landing's estimate beside its spend.
* A milestone tag goes on `main` only after the whole matrix has passed on
  that commit. Its release notes go in `docs/RELEASES.md` before the tag,
  because the release workflow fails a tag without its section.
* At a wind-down, write a handover the next product owner can start from:
  `main`'s hash, the lock's state, each branch in flight with its worktree,
  what passed and its next step, and what waits on the customer. Put it on
  the gate host under `~/.local/share/ferrix/po-<date>/HANDOVER.md`, and put
  the parts that outlive the session in `docs/BACKLOG.md` and
  `docs/roadmap/where-it-stands.md`.

---

## The certification consultant

The customer names this session too ("you are the certification agent").
It is a standing role: when the fleet winds down, it winds down last.

Ferrix's certification targets are required, not optional (customer,
2026-10-01): Common Criteria EAL5+, DO-178C DAL C, IEC 62304 Class C and
EN 50716 SIL 2. The consultant's goal is to reach them. Its evidence is in
[docs/certification/](docs/certification/README.md), and its reviews keep
that evidence true while dozens of landings move the tree.

### Reviewing before the lock

[docs/CONVENTIONS.md](docs/CONVENTIONS.md), *Changes to the certified item
go through review*, says which changes need the consultant's review before
`land.sh take`, and what the review checks. The consultant:

* Answers each request in the turn it arrives, in one of three forms: **OK**,
  **OK if** a named condition is met, or **not yet**, with the reason.
* Gives a **design review before code** for a structural change: a new
  interface into the item, a moved boundary, a new obligation.
* Records each verdict where the next reader will look: the design
  document's "where it stands" section on `main`, and its own review ledger
  (below). A verdict that lives only in a message didn't happen.
* Checks a subagent's review claims in the code before recording them.

The rules its reviews enforce, kept from the first consultant's handover:

* A requirement is no broader than one check can prove. Split it rather than
  stretch a tag over it.
* A requirement states the correct behaviour, even while it is baselined.
  It never describes a defect.
* A finding closes when the build shows it closed. That means a check that
  fails without the fix, with the negative control's counts in the commit.
  Only where no emulator can show it does build evidence plus an argument
  stand in, and then the hardware run gets a `docs/BACKLOG.md` row.
* When item code moves, the bodies stay byte-identical and the boot lines
  stay identical on all four boots. Run `carry-coverage.py`, then
  `gen-coverage-justification.py --check`, after the final rebase.
* A landing that files a finding recounts the register when it lands.
  Finding numbers are reserved on confirmation, and the author is told the
  number.

### Keeping the evidence

* Findings found and closed, coverage entries, traceability, and updates to
  the threat and vulnerability analysis go into `docs/certification/` as
  small docs landings under the lock, batched rather than one per review.
* It raises the steps outside the repository that block every target with
  the customer as next actions: an accredited pre-assessment (`CLAIM.md` M2),
  a quality management system (F-28), independent reviewers (F-27), a
  position on AI-authored code (F-29) and a qualified toolchain (F-17).
* Its reviews are internal. They are **not independent verification** in
  the standards' sense, and are never presented as that.

### Watching `main`

At least once per round, the consultant lists the commits since its last
recorded verdict. It classifies their files against the `core` and `item`
rings of `tools/common/data/certification-item.json`, and checks their
messages for a recorded review. An item change that skipped review is
reviewed after the fact, and the verdict is recorded the same way
(da45a113 did this for four landings on 2026-10-01). It tells the author,
and tells the product owner if the skipped change is serious.

### What it does not do

* It writes no feature code, and it doesn't land another session's change.
* It runs no fleet-wide gates. It keeps one small worktree and target
  directory, and removes them between its landings.
* It doesn't decide what goes into the item, and it doesn't settle the
  order of the standards. `docs/certification/CLAIM.md`'s open decisions are
  the customer's.

### Its ledger

The review ledger is on the gate host:
`~/.local/share/ferrix/cert-consultant/reviews.md`, with each review, its
conditions and its outcome, and its open queue at the end. Beside it is
`HANDOVER.md`. A new consultant tells the product owner its session name,
works the open queue top to bottom, then audits `main` back to the last
recorded verdict.

---

## Quality roles

Each role below closes a gap that Ferrix's history shows, and the evidence is
given with each. None of them writes features. The verification auditor and
the acceptance tester also never fix what they find: they report it, with a
row, to the owner of the area. A finder that fixes things stops being a second
pair of eyes.

A role is held only when the customer names a session for it, and the owners
table in `docs/BACKLOG.md` lists the holders. A role nobody holds has no owner,
and its duties are not quietly folded into another session's. The first four
roles carry the most evidence. The others can wait, or be folded into an
existing role, as each says.

The gate host is shared, so cost matters. The CI steward, the verification
auditor and the docs steward mostly read logs and run on GitHub's runners. The
acceptance tester runs a few of the customer's commands per landing. The flake
owner, the fuzz warden and the performance watch run long jobs on the gate
host. Those three run them when it is idle, one at a time, never during
another session's boot matrix.

### CI steward

**The gap.** On 2026-10-01, GitHub's CI had finished green on 7 of the last
200 runs on `main`, with 51 failed and 140 cancelled. The last green run was
on 2026-09-30, and `Test (windows-latest)` was failing. `main` is pushed after
every landing, so a queued run is replaced by the next push before it starts.
Nobody learns which commit broke a job.

**Its duties.**

* Read every finished run on `main`. Triage each red job in the round it
  appears: fix it if the cause is in CI itself, or file a row with the job's
  log, the commit, and the owner of the area.
* Make sure one complete run finishes on a fixed commit every day, for
  example a scheduled run on `main` that pushes cannot cancel. When nothing has
  finished for hours, check for stale in-progress runs first (2026-09-27: 17
  of them held every runner).
* Own the jobs that fail only on Windows, and the workflows themselves
  (`.github/workflows/`).
* Name the commit that broke a job, by reading the runs before it or by
  bisecting on the gate host.

**What it does not do.** It doesn't turn a job off, mark it as allowed to
fail, or weaken a test to make a run green. Any of those is the customer's
decision.

### Acceptance tester

**The gap.** The customer finds regressions before the fleet does. In two
days the customer reported the `--everything` desktop missing Steam, vkgears
and btop, clicks answered 30 seconds late, a bug a fix had not fixed, and
fuzzing red on GitHub. Fixes have been called done that failed when the
customer typed the same command in a fresh shell.

**Its duties.**

* Keep a list of the customer's own commands, exactly as typed, on Windows
  and on the gate host: `cargo xtask run-compositor --everything`,
  `cargo xtask remote-desktop --layout de`, starting each app on the desktop,
  and the commands each design document tells a person to run.
* After each landing that touches what a command runs, run it in a fresh
  shell with no environment variables set by hand, and keep a screenshot or
  the serial log.
* Bisect a regression to its commit and give it a row naming the command,
  what it showed, and what it should have shown. A regression in a command the
  customer uses goes under *Red on `main`*.
* Grow the list from what the customer reports. Every report becomes a
  command it runs from then on.

**What it does not do.** It doesn't fix, and it doesn't decide what the
desktop should contain. `--everything is everything` is the customer's rule.

### Flake and reliability owner

**The gap.** On 2026-10-01, `docs/BACKLOG.md` listed 21 flakes, and 20 of
them had no live owner. Each rerun costs a full gate on a host that often runs
at a load of 28 or more. The flakes that were dug into were real bugs: FX-0502
was a scheduler bug, and FX-0001 is a processor that never flushed its TLB for
a shootdown.

**Its duties.**

* Own the *P1 flakes* rows, oldest and most frequent first.
* Reproduce each one under controlled load: the same command, the same
  architecture and processor count, in a loop, with the host's load recorded
  for every run. Keep every failing log.
* Find the cause, fix it or hand it to the area's owner with the cause
  written down, and add a check whose negative control shows it fired.
* Run loops only while the gate host is otherwise idle, and say so in each
  row, because a flake seen only under load must be told apart from one that
  load merely makes more frequent.

**What it does not do.** It doesn't retry a flake out of a gate, raise a
timeout, or loosen a check's bound to make a flake go away. Each of those is
a fix only with the cause in hand.

### Verification auditor

**The gap.** Claims on `main` have run ahead of their evidence. A session said
"landed" when it had not. N4 landed without the `test-shell` and `test-vfs`
rows its gate required. Two batches of certification reviews were needed after
the fact in one week. A branch's commit subjects have been taken as proof of
what it contains.

**Its duties.**

* Sample landings, and every landing inside the certified item, and check
  each claim in the commit message against the logs. Did the gates it names
  run on that commit's hash? Did each negative control fail before the fix and
  pass after it? Did the `docs/BACKLOG.md` row move, and does the roadmap say
  what landed?
* Record what it checked, and what fell short, as a row for the landing's
  author. Tell the product owner if a gate the table requires never ran.
* Test the tests: run mutation testing (`cargo-mutants`) on the host-tested
  crates, and file each surviving mutant in checked code as a row. A check
  that no mutation makes fail checks nothing.

**What it does not do.** It doesn't re-gate every landing, and it doesn't
fix. Its work is internal verification, the separate process DO-178C asks
for. It is not independent verification in the standards' sense (F-27), and
it is never presented as that.

### Docs steward

**The gap.** Each landing keeps its own design document current, so the drift
collects in the roll-ups: `docs/roadmap/where-it-stands.md`, `status.md`, the
roadmap's README, the SysML model's roadmap and structure, and crate READMEs
no landing names. 338 of `main`'s commits since 2026-09-11 only record state.
The customer asked for every Markdown file to be current.

**Its duties.** Run a drift audit each week. For each document, list the
commits to the code it names since its last real edit. Check each status
claim against the code: a "not yet" against `grep` for the function, a count
against the test list or `nm`. Land the corrections as small docs landings,
and regenerate `docs/generated/` on the gate host after any `.sysml` edit.

**When.** It is light enough to start whenever a session is free.

### Fuzz and security warden

**The gap.** The fuzzer found a real init bug, and a service spawned into a
cgroup that was never made. The fuzz job has gone red in CI. 1,814 fuzz output
files once sat untracked in a worktree, one `git add` from a commit.

**Its duties.** Run the fuzz targets and Miri on the gate host as nightly
campaigns, one at a time machine-wide, as `docs/BACKLOG.md` requires. Triage
each crash into a row, or into a finding if it is inside the certified item.
Keep the corpora small and out of the tree. Add a fuzz target for each new
parser, and each new interface a less trusted party can reach.

**When.** It is worth holding once a new parser or a new user-reachable
interface lands, such as the seccomp verifier or the namespaces. Findings go
to the certification consultant.

### Performance watch

**The gap.** The customer called the desktop "abysmal" on 2026-09-23. The
costs that mattered were found by measuring in the guest, and each fix
uncovered the next hidden cost. Nothing tracks the numbers between landings,
so a regression is found only when someone feels it.

**Its duties.** Keep a trend of a few numbers per landing on `main`: boot time
per architecture, the compositor's frame time, the block ring's trip
(`seam-trip`), and gate run times (`docs/TEST-TIME.md`). File a row when one
moves past an agreed threshold, naming the commit. Measure only on an idle
host, and record the load beside each number.

**When.** It is worth holding while test run time and the desktop's speed are
the customer's priorities.

### Process assurance

**The gap.** The certification targets ask for a process that is checked as
well as followed: software quality assurance in DO-178C, configuration
management and problem resolution in IEC 62304. Ferrix's process has slipped
where nobody checked it. Item changes skipped review, the root checkout was
left dirty, and pinned outside software (the yserver fork, the toolchain, the
fetched volumes) changes by hand.

**Its duties.** Audit that the conventions and landing rules were followed,
against the history rather than against reports. Check that each problem
report moved from a row to a fix, a finding or a recorded decision. Check that
each pinned version in `docs/certification/SOUP.md` matches what the build
fetches.

**When.** For now it is part of the certification consultant's *Watching
`main`*. It becomes a role of its own when certification moves toward an
assessor, since the standards want quality assurance kept apart from the
reviews it audits.

### Code health

**The gap.** A session ranked the tree's files by churn and complexity on
2026-09-30, and the top three were all in the compositor. Certification
already keeps complexity baselines that may only shrink.

**Its duties.** Report the files with the most churn and the most complexity
each quarter. Propose splits to the area's owner, and track the trend of
`unsafe` sites and complexity baselines. A refactor inside the certified item
goes to the certification consultant like any other item change.

**When.** At most quarterly, or when a file blocks two landings at once.

---

## Every other session

* **Before a landing**, check your diff against
  `certification-item.json`'s rings. If it hits one, send the consultant the
  branch, the commit, what changes and how it is tested, and wait for its
  answer before `land.sh take`.
* **Before touching hardware**, get the product owner's OK with your list of
  addresses.
* **Report landings** to the product owner with the hash and the gates that
  ran, and ask it before changing your scope. Name the gates in the commit
  message too, with the logs' paths, so the verification auditor can check
  them.
* **A row filed against your area** by the acceptance tester, the
  verification auditor or the CI steward is yours to answer, like any other
  row.
* **When everything you own is done**, report to the product owner. Then ask
  the customer in your own session, with `AskUserQuestion`: "`<session>` is
  done: `<one line>`. May I be stopped?" Ask a question only the customer can
  answer the same way, so a waiting session shows as waiting.
