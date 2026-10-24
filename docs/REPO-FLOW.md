# Repository flow: develop, promotion to main, two lanes

How a change reaches `main` (customer, 2026-10-23). In short:

* The fleet lands on **`develop`**, as it landed on `main` before, through
  `land.sh` and `batch.sh`. The gate pool's rows run before the landing, as
  before.
* **`main` moves only by promotion.** A workflow cuts a promotion branch
  from `develop`, CI runs once on it, and the workflow fast-forwards `main`
  to it when its checks pass. No pull request is opened, so the owner gets
  no review request per promotion (customer, 2026-10-24). Nobody clicks
  anything on the owner's side.
* **Two lanes**, chosen by who opened the pull request, never by its branch
  name. The **owner lane** (the customer's account, the fleet, which pushes
  as it, and the repository's own workflows) is fully automatic. The
  **outside lane** (forks, anyone without write access) never merges by
  itself: it needs full CI, the certification consultant's review, a
  security review of the diff, and the owner's approval.
* **CI runs where a verdict is needed**: on pull requests to `develop`, on
  each promotion, and on `develop` once a night. It no longer
  runs on every push.

Most of the design is plumbing to keep lead time short. A finished
landing is on `main` 34 minutes later in the median, and under an hour at
the 90th percentile (*Lead time*). Branch names are prefixed, and
outdated branches were archived on 2026-10-23 (*Branch clean-up*).

## Branches

| Branch | What it is | Who moves it |
|---|---|---|
| `main` | What has passed CI and the certification verdict. Releases and tags are cut from it. | The Promote workflow, as a fast-forward to a promotion whose checks passed. No other pushes. |
| `develop` | The integration branch. Every landing goes here. | The fleet, under `land.sh`. Outside pull requests also go here, after review. |
| `promote/<sha>` | One promotion: `develop` at `<sha>`, where CI runs before `main` moves to it. | The Promote workflow. It is deleted once `main` holds it, or once a newer promotion supersedes it. |
| `feature/<what>` | New behaviour. | Its author. |
| `bugfix/<what>` | A fix for something that is broken on `develop` or `main`. | Its author. |
| `chore/<what>` | Tooling, CI, refactors, dependency bumps: no behaviour change. | Its author. |
| `docs/<what>` | Documentation only. | Its author. |
| `release/<version>`, `hotfix/<what>` | Kept free for a release that needs stabilising away from `develop`, and for a fix that must reach `main` before `develop` can be promoted. Unused so far. | The product owner. |
| `wip/<what>` | Pushed to keep it safe, not ready for anyone else. | Its author. |
| `park/<what>` | Stopped on purpose and kept for later. A handover or memory note names it. | Whoever parked it. |

A branch name is lower case, with `-` between words and `/` only after the
prefix. A stream may add its own group after the prefix
(`feature/os7s/k14-empty-chain`). Agents name their worktrees and branches
this way from now on: `git worktree add -b feature/<what>
.claude/worktrees/<what> develop`.

**Stale branches.** A pull request's branch is deleted when it merges (the
repository's *automatically delete head branches* setting). Once a month, the
product owner sweeps the rest. A branch whose commits are all on `main` is
deleted. A branch with no commit for 14 days, that no handover or memory note
names, is archived. Before any name is removed, its tip is saved as
`refs/archive/<date>/<name>`, which is how the 2026-10-23 clean-up worked (see
below). GitHub calls a branch stale after three months without a commit
([viewing branches][gh-branches]). We use 14 days because agents branch
far faster than people do.

## Two lanes

The lane is decided by `tools/common/release/flow-lane.py`, which
`.github/workflows/flow.yml` runs on every pull request event. It reads two
facts that a contributor cannot choose:

1. **Is the head branch in this repository?** A fork's never is. Only an
   account with write access can push a branch here.
2. **What permission does the author have** on the repository, according to
   the API? It must be `admin`, `maintain` or `write`. The repository's own
   `github-actions[bot]` also counts, because it authors the promotions.

Both true: owner lane. Otherwise: outside lane. A branch name, a title, a
label the contributor set, or a claim in the description never changes the
lane. The result is the status **`flow/lane`**, which the rulesets require.

### The owner lane: automatic, minimal

For the fleet and the customer.

* The fleet does not open pull requests for its landings. It lands on
  `develop`, and the Promote workflow carries that to `main`.
* An owner pull request opened by hand, to `develop` or `main`, gets
  `flow/lane` success at once, and auto-merge is switched on. It merges when
  `CI result` and `certification/verdict` pass.
* The certification verdict is worked out automatically (see *The
  certification verdict*). Docs, tooling and userland changes do not touch
  the item, so they need only green CI.
* No human approval is required. GitHub would not allow one anyway: the
  author of a pull request cannot approve it ([GitHub docs][gh-approve]),
  and every owner pull request is the customer's own.

### The outside lane: reviewed, never automatic

For everyone else (customer, 2026-10-23: "they will need a full review with
extra care so that no one smuggles in malware"). Outside pull requests
target `develop`, as CONTRIBUTING.md asks. Before one can merge:

1. **Its workflows wait for a maintainer.** GitHub's *Require approval for
   all external contributors* setting applies to every fork pull request,
   not only a first one ([Actions settings][gh-fork-approval]). Read the
   diff before approving the run: CI runs the contributor's code.
2. **CI runs with nothing to steal.** `ci.yml` runs on `pull_request`, never
   `pull_request_target`, with `permissions: contents: read` and no secrets
   ([security hardening][gh-hardening], [preventing pwn requests][gsl-pwn]).
   `flow.yml` is the one workflow with a write token on a fork's pull
   request. It checks out the base branch, fetches the pull request's
   commits only as git objects, and only reads them.
3. **Full CI**: `CI result`, which includes the dependency review of what
   the pull request adds to `Cargo.lock` against GitHub's advisory database
   ([dependency review][gh-depreview]).
4. **The consultant reviews every outside pull request**, whatever it
   touches. `certification/verdict` stays pending until the consultant posts
   `cert-verdict.py --verdict OK`.
5. **Supply-chain flags.** `tools/common/check/check-supply-chain.py` posts a
   comment, refreshed on every push, listing where a smuggled change would
   hide:
   * files that run at build or CI time: `.github/`, `.githooks/`,
     `build.rs`, xtask, scripts, Cargo manifests, `.cargo/`, toolchain
     files, `components.toml`, fetch scripts and checksums, `.claude/`,
     `vendor/`;
   * new crates in a lockfile, with any name one or two edits from a crate
     already locked (typosquatting);
   * binary files;
   * bidirectional and invisible Unicode ([Trojan Source][trojan]);
   * lines over 400 characters and long base64 or hex runs;
   * added lines that reach the network, start processes or embed files.
6. **An AI security review of the diff.** On the gate host, brief a review
   subagent with the diff, the flags comment, and this list: backdoors,
   hidden network access, build-time code execution, Unicode and bidi
   tricks, typosquatted or newly added crates, changes to CI or pins,
   obfuscated code, tests weakened to let something through. Post its
   findings as a comment on the pull request. It reads the diff as text and
   never builds it.
7. **The owner approves and labels.** The owner approves the head commit
   (CODEOWNERS names the owner on every path, and the `develop` ruleset
   requires a code owner's approval of the last push). The owner reads the
   security review, then puts on the label **`security-reviewed`**. Do it in
   that order: the label is what makes `flow.yml` check again. `flow/lane`
   turns green only when a maintainer's approval of this commit and a
   maintainer's label both stand. Every new push takes the label off.
8. **The owner merges by hand.** Auto-merge is never switched on for this
   lane.

Commit signing: the maintainers sign their own commits once the signing
key is set up ([commit signature verification][gh-signing]). The fleet's
commits are not signed yet, so no ruleset requires signatures. Requiring
them would block every promotion until the fleet signs.

## Promotion: develop to main

`.github/workflows/promote.yml` runs on every push to `develop`, when a
promotion's CI ends, and every 15 minutes as a fallback:

1. A promotion branch `promote/<sha>` whose CI passed, whose `CI result`,
   `flow/lane` and `certification/verdict` statuses are green, and that
   `main` is an ancestor of, is pushed to `main` as a **fast-forward**, and
   the branch is deleted. `main` then equals `develop` at `<sha>`: the same
   hashes, no merge commit, and the next promotion is always a
   fast-forward again.
2. While a promotion's CI is running, nothing new starts: the next
   promotion takes everything that lands in the meantime, and a landing
   never cancels a run halfway through.
3. A promotion whose CI ended and that cannot reach `main` (red, or its
   verdict waiting or NOT-YET) stays while it carries `develop`'s tip, so a
   late verdict still lets it through. Once `develop` has moved past it, it
   is deleted and the new tip is promoted. A promotion never holds back
   what landed after it.
4. Otherwise, if `develop` has commits `main` lacks, the workflow cuts
   `promote/<sha>` at `develop`'s tip, posts that commit's `flow/lane`
   (owner lane) and its `certification/verdict`, and dispatches `ci.yml`
   on the branch. CI's last job posts its verdict as the commit status
   `CI result`.

**No pull request** (customer, 2026-10-24). A promotion pull request made
GitHub request a review from the code owner on every promotion:
`CODEOWNERS` is read from `main`, which carries `develop`'s file, and
names the owner for everything. Its inbox filled with promotions, most of
them superseded. `main`'s rules are split in two rulesets for this:

* **protected main**: no deletion, no non-fast-forward, and the three
  required checks (`CI result` from the GitHub Actions app, `flow/lane`,
  `certification/verdict`) on any commit pushed. Nobody bypasses it, the
  workflow included.
* **main via promotion**: the pull-request rule, which the GitHub Actions
  app (the workflow's token) bypasses. Anyone else still needs a pull
  request to change `main`.

Everything in the workflow uses its own token, so the push to `main`
starts no push run there, and CI never runs a second time on `main` for a
commit that was already checked ([GITHUB_TOKEN][gh-token]).

### Lead time

Measured against the 225 landings of 2026-10-09 to 2026-10-22, each taken
as a push to `develop` at the time it reached `main`. The simulation used
CI's measured wall time: median 21 minutes, 90th percentile 35, over the
same days.

| CI wall time per promotion | Median, landed → on `main` | 90th percentile | Worst |
|---|---|---|---|
| 25 min | 34 min | 49 min | 52 min |
| 35 min | 52 min | 69 min | 72 min |

"Agent done" is when the gate pool passes the landing and it is on
`develop`, the same moment it reached `main` before. A promotion that goes
red holds `main` back until a fix lands on `develop`. The fix is promoted
at once (step 1's exception). So a red `develop` is now the product owner's
first job in each round, as a red `main` was before.

### Releases and tags

Tags go on `main` (`v*`, `stage-*`), and `release.yml` is unchanged. A
release that needs stabilising while `develop` moves on gets a
`release/<version>` branch cut from `main`. Fixes go there by pull request,
then back to `develop` ([about releases][gh-releases]).

## The certification verdict

The required status **`certification/verdict`** stands for the consultant's
approval. It is a status and not a review, because a pull request's author
cannot approve their own pull request ([GitHub docs][gh-approve]) and every
owner pull request is the customer's. An outside contributor has no write
access, so cannot post one. No second account and no GitHub App are
needed.

`tools/common/release/cert-verdict.py <base>..<head>` works the verdict out.
It finds the commits that touch the certified item with the consultant's
own `item-hits.py`, which resolves rings the same way as
`check-item-boundary.py`:

* **Owner lane.** If no commit touches the item: success. If every one that
  does names its review in its message (a ledger line such as `ledger
  2692`, a standing pattern's `SP-1:` check-list, the consultant, or a line
  `Certification: <why none is needed>`): success. Otherwise the verdict is
  pending, and the pull request lists those commits until the consultant
  answers. The answer is `cert-verdict.py <range> --verdict OK --note
  "ledger <line>" --post <sha>`, posted with the owner's token from the gate
  host, or `--verdict NOT-YET`, which posts failure.
* **Outside lane.** Pending until the consultant answers, every time.

An automatic post never replaces a verdict a person posted on the same
commit. The review itself does not change: AGENTS.md's consultant section
and CONVENTIONS.md's rules still decide what needs review before `land.sh
take`. The difference is that the review must now show in the commit
message, so that the promotion can see it. Over `main`'s last 150 commits,
14 of the item commits named no review: roll-ups, reservations and fix-ups
after a reviewed commit. From the switch on, such a commit carries a
`Certification:` line (for example `Certification: not needed, roadmap
roll-up only` or `Certification: fix-up of ledger 3170's landing`). Run
`cert-verdict.py develop..<branch>` before `batch.sh join`, so a promotion
is never held on a line that is missing. The consultant audits these lines
on `main` afterwards, the way it audits standing patterns.

## CI: what runs when, and what it saves

Measured on 2026-10-23 with `gh run list --workflow ci.yml`:

* From 2026-09-12 on: 746 runs. 670 were pushes to `main`, 67 pushes to
  `develop` (before 09-17), 8 pull requests and 1 dispatch.
* The last 14 days: 226 runs, **16.1 a day** (busy days 30 to 44, and 85 on
  09-27), every one a push to `main`. Results: 45 success, 129 failure, 51
  cancelled.
* One full run is 20 jobs and about **128 job-minutes**. Wall time: median
  21 minutes, 90th percentile 35.

The new triggers:

| Event | Runs CI? |
|---|---|
| Push to `develop` (each landing) | No. The gate pool ran the landing's rows. |
| Promotion `promote/<sha>` | Yes, once, by dispatch. This is the run that lets it merge. |
| Pull request to `develop` | Yes. Outside ones wait for a maintainer's approval first. |
| Any other pull request to `main` | Not by itself: GitHub would hold a promotion's `pull_request` run for approval (its pull request is opened with the workflow token), so `main` is left out of the trigger. Start it with `gh workflow run ci.yml --ref <branch> -f base=<merge base>`, as Promote does. |
| Merge of a promotion into `main` | No: the workflow token's push starts nothing. |
| Direct push to `main` (an administrator, after the switch) | Yes. |
| `develop`, nightly at 02:17 UTC | Yes, if `develop`'s tip is not in `main` and has not run yet. |
| Tags | `release.yml`, unchanged. |

**Saving.** A promotion takes every landing that arrives while the one
before it is running. The simulation above gives **9.5 to 11.1 runs a day,
against 16.1**: 31 to 41% fewer full runs, about 640 to 840 job-minutes a
day. The saving is biggest on busy days, when landings arrive faster than CI
finishes. On a day like 09-27 (85 landings), CI is capped near one run per
promotion cycle, about 30 a day. Cancelled runs (51 in 14 days) also stop:
a promotion's run is never replaced halfway through. The repository is
public, so standard runners cost no minutes. What the saving buys is queue
time and a verdict for each commit that reaches `main`.

**One required check.** The rulesets require only `CI result`, a job that
waits for all the others. It fails if any of them failed, so the required
list does not change every time a job is added. **"rustc on Ferrix" is
advisory for now.** On the hosted runner's KVM it fails most runs with two
kernel self-check panics that no other host shows:

* `FERRIX-PANIC stage 7 self-check failed: getrusage(RUSAGE_THREAD) counted
  more time than passed on the wall clock` (runs 39930865642,
  39921684010). It has no backlog row yet.
* The stage 9 XINUSE wake check (runs 39927490414, 39814853954), which
  `docs/backlog/flakes.md` marks done since 2026-10-16.

As long as these fire, a required check that included this job would stop
every promotion. Remove `rustc` from `ADVISORY` in `ci.yml` once both are
fixed.

**Not adopted, and why.**

* **Merge queue** ([managing a merge queue][gh-mq]) batches pull requests
  and runs CI on each group, as `batch.sh` does locally. With one promotion
  at a time there is nothing to queue. It would earn its place if many
  owner pull requests went to `main` at once. It needs CI to listen to
  `merge_group`.
* **Path-filtered CI** for docs-only pull requests. A required check that a
  path filter skips never reports, and the pull request blocks. Doing it
  properly means a skip inside `CI result`. Worth doing if outside docs
  pull requests become common.

## GitHub settings

`tools/common/release/github-flow-setup.sh` makes every settings change
(the owner runs it; agents never do). Without `--apply` it only prints
what it would do.

* **`setup`** is safe at once, while the fleet still lands on `main`:
  * auto-merge on, merge commits and squash on, rebase merge off, head
    branches deleted on merge;
  * Actions' default token read-only, and no workflow may approve a pull
    request;
  * fork pull requests from all external contributors wait for approval;
  * secret scanning with push protection ([push protection][gh-push-prot])
    and Dependabot alerts;
  * the `security-reviewed` label;
  * the **`develop` ruleset**: no deletion, no force-push, and a pull
    request needs `CI result`, `flow/lane`, `certification/verdict` and a
    code owner's approval of its last push. Repository administrators
    bypass it, so the fleet's landings still push straight to `develop`
    ([rulesets][gh-rulesets], [available rules][gh-rules]).
* **`switch`**, at the switch: **`protected main`** keeps no deletion and
  fast-forward only, and gains the three required checks, not in strict
  mode; nobody bypasses it. A second ruleset, **`main via promotion`**, adds
  the pull-request rule (merge commits only, no approvals), which the
  GitHub Actions app (integration 15368, the Promote workflow's token)
  alone bypasses, so the workflow fast-forwards `main` to a promotion whose
  checks passed and opens no pull request (2026-10-24). Running `switch`
  again applies a changed rule set. It also sets the repository variable
  `FLOW_PROMOTE=on`; the Promote and Nightly workflows do nothing without
  it. `undo-switch` restores the old ruleset, deletes the second one and
  removes the variable.
* **`show`** prints the current state.

## The switch, step by step

The product owner does this at a moment when no agent is mid-landing:

1. Land this branch's tooling commit on `main` the usual way. It is inert:
   `land.sh` and `batch.sh` read `fleet/land-branch`, and while that file
   is missing they mean `main`.
2. Move `develop` to `main`. The old `develop` (39e8fabc1, 2026-09-17,
   two commits never landed) is saved as `refs/archive/2026-10-23/develop`
   on origin. The agent's force-move was refused, so this step is the
   owner's:
   `git push --force-with-lease=refs/heads/develop:39e8fabc1b0133704f67c3ba426e00ce8f24389c origin <main>:refs/heads/develop`.
3. The customer runs `github-flow-setup.sh setup --apply`.
4. `land.sh status` shows FREE with an empty queue, and no batch is
   running (`batch.sh status`).
5. `echo develop > ~/.local/share/ferrix/fleet/land-branch`. Point the gate
   host's and the root checkout's `develop` at `origin/develop`.
6. The customer runs `github-flow-setup.sh switch --apply`.
7. Land the process-docs commit (AGENTS.md, CONVENTIONS.md, BACKLOG.md and
   the product-owner skill) on `develop`. The first promotion carries it to
   `main`.
8. Record the switch under *Decisions* in `docs/backlog/DECISIONS.md`.

Undo: `undo-switch --apply`, `rm fleet/land-branch`, and land on `main`
again.

## Best practice consulted

| Topic | What GitHub (or the common practice) says | What Ferrix does |
|---|---|---|
| Branching model | GitHub Flow: short branches off the default branch, merged by pull request ([GitHub Flow][gh-flow]). Git-flow: a `develop` integration branch with `feature/`, `release/` and `hotfix/` branches ([nvie][gitflow]). Trunk-based: everyone integrates into one branch at least daily, behind feature flags ([trunkbaseddevelopment.com][tbd]). | A hybrid. Trunk-based on `develop`, where the fleet integrates many times an hour under its own gates. `main` is a continuously promoted, CI-verified trunk, with no fixed release train. Git-flow's prefixes are used for names, but there are no long-lived feature branches. |
| Protecting branches | Rulesets replace branch protection. They layer, can be read by anyone with read access, and support bypass lists ([about rulesets][gh-rulesets]). | Rulesets on `main` and `develop`. Classic branch protection is not used. |
| Required checks | A pull request merges only when the named checks pass. *Strict* also requires the branch to be up to date ([protected branches][gh-protected]). | One aggregate check from CI plus two statuses. Not strict, for the reason given under *GitHub settings*. |
| Merging without a person | Auto-merge merges once requirements are met ([auto-merge][gh-automerge]). A merge queue also tests pull requests in groups ([merge queue][gh-mq]). | Auto-merge. A merge queue is kept for later (*Not adopted*). |
| Ownership | CODEOWNERS requests and, with a ruleset, requires code owner review ([code owners][gh-codeowners]). | CODEOWNERS names the owner on every path, and again on the build and trust paths. Required on `develop` for the outside lane. |
| Contributor guidance | CONTRIBUTING, pull request templates, issue forms and community health files ([guidelines][gh-contrib], [PR template][gh-prtemplate], [issue forms][gh-issueforms], [health files][gh-health]). | CONTRIBUTING.md gained the pull request flow. `.github/pull_request_template.md` added. The issue forms (bug, Linux program) and SECURITY.md were already there. |
| Labels | Labels sort and route issues and pull requests ([labels][gh-labels]). | `security-reviewed` is the one label with a meaning in the flow. Triage labels are for the product owner to add when outside issues arrive. |
| Untrusted code in CI | Fork pull requests get a read-only token and no secrets under `pull_request`. `pull_request_target` must never run the pull request's code ([hardening][gh-hardening], [pwn requests][gsl-pwn]). Approve fork workflow runs ([settings][gh-fork-approval]). | As described in *The outside lane*. |
| Dependencies | Dependency review on pull requests ([dependency review][gh-depreview]). cargo-vet records audits of each crate version ([cargo-vet][vet]). | Dependency review in CI, cargo-deny as before, and new crates flagged for the reviewer. cargo-vet is a later step: it needs an audit set to be built first. |
| Signed commits | GitHub shows commits as verified and rulesets can require signatures ([signature verification][gh-signing]). | The maintainers sign. A requirement waits until the fleet signs (*The outside lane*). |
| Stale branches | GitHub lists branches with no commit in three months as stale. Head branches can be deleted on merge ([branches][gh-branches], [automatic deletion][gh-autodelete]). | Deleted on merge. A monthly sweep with archive refs (*Branches*). |

## Branch clean-up of 2026-10-23

Every branch in the root checkout on nazuna, and on `origin` (the only
remote), was classified by `git patch-id` and commit subject against
`origin/main`, by age, and by whether any memory note, handover
(`~/.local/share/ferrix/handoff/`, `docs/handover/`) or backlog file names
it. The full map is
[`docs/repo-flow/branch-map-2026-10-23.tsv`](repo-flow/branch-map-2026-10-23.tsv):
one row per branch, giving its old name, its action, its new name, its
archive ref, its tip, its last commit date and its unlanded commits.

| Where | Deleted (all landed) | Archived (unlanded, stale) | Renamed | In use, left alone | Kept |
|---|---|---|---|---|---|
| Local (1,127 before, 348 after) | 554 | 225 | 194: 117 `feature/`, 57 `park/`, 19 `chore/`, 1 `docs/` | 152 | 2 (`main`, `develop`) |
| origin (600 before, 263 after) | 137 | 321 | 99: 72 `park/`, 26 `feature/`, 1 `docs/` | 29 | 13 (`main`, 11 `wip/`, `develop`*) |

\* `develop` is still the 2026-09-17 one, until switch step 2.

The rules:

* **Delete**: every commit of the branch is on `main` by ancestry, patch-id
  or subject.
* **Archive**: unlanded, no commit for 14 days or more, named in no note.
  Most are stack, backup and pre-rebase copies (`os7s/`, `os-cgctl/`,
  `os07/`, `backup/`, `po10-pipe/`).
* **`park/`**: unlanded, 14 days or older, named in a note. These are the
  parked branches the customer wants kept.
* **`feature/`, `bugfix/`, `chore/`, `docs/`**: unlanded and younger than 14
  days. The prefix was guessed from the name. Rename again freely.
* **In use**: checked out in a worktree (116), named in the brief as active
  (`gpu-opaque-2`, `sched-l73*`, `hyprix-gnome`, `kernel-crates-s3*`,
  `linux-abi*`, cpuinfo), an unlanded commit in the last 48 hours (it may be
  live on another machine), a Codex PR branch, or held by a worktree
  mid-rebase (`nvme-driver`).

Nothing is lost. Before a name was removed, its tip was saved:

* an origin branch: as `refs/archive/2026-10-23/<old-name>`, on origin and
  in the root checkout (558 refs, the old `develop` among them);
* a local branch: as `refs/archive/2026-10-23/local/<old-name>`, in the
  root checkout only, as the brief asked.

To restore one: `git push origin
refs/archive/2026-10-23/<old-name>:refs/heads/<old-name>`, or `git branch
<old-name> refs/archive/2026-10-23/local/<old-name>`. GitHub does not show
`refs/archive/*` in its interface. `git ls-remote origin 'refs/archive/*'`
lists them. The earlier sweep of 2026-10-10 lives beside them, under
`refs/archive/2026-10-10/`.

**Second sweep, the same day** (customer: GitHub down to about a hundred
branches). Map: [`docs/repo-flow/branch-map-2026-10-23b.tsv`](repo-flow/branch-map-2026-10-23b.tsv);
archive refs under `refs/archive/2026-10-23b/`, all pushed to origin (a
local branch whose tip differs from origin's under `.../local/<name>`).

* Archived without a backlog line: the 2026-09-24 "before switching PCs"
  `wip/` snapshots and every branch whose commits say "not for landing",
  "never lands", "measurement only" or "diag" (50 on origin, 48 local).
* Archived after one "idea:" line each in `docs/backlog/branches.md`
  (br-006, landed first as ff517bef8): every `feature/`, `wip/`, `mk/`
  and `chore/` branch with no commit for seven days that no running agent
  uses (102 on origin, 89 local).
* Deleted, all on `develop`: 3 on origin, 2 local.
* Prefixed: the stragglers (`mk/*`, `xelp-*`, `fs-traits`,
  `lockup-s4a-wip`, `codex/posix-stdlib`, ...) became `feature/...` (18 on
  origin, 25 local).
* Kept: `park/*` (75), `main`, `develop`, `promote/*`, and the active
  agents' branches and every branch checked out in a worktree.

Origin by prefix, before and after: feature 124 -> 23 (the renamed
stragglers included), park 75 -> 75, chore 20 -> 6, mk 13 -> 0, wip 11 ->
0, docs 2 -> 2, promote 2 -> 2, unprefixed 14 -> 2 (`main`, `develop`),
other groups 3 -> 0; 265 -> 110, of which 75 are parked, not counting
branches pushed since.

**Left for later.** Of the 116 branches checked out in worktrees, 37 are
fully landed and 9 are unlanded and older than 14 days. Their worktrees
can go, with their branches, once their sessions confirm they are done. The
next sweep handles them, along with renaming the in-use branches when they
are free.

[gh-flow]: https://docs.github.com/en/get-started/using-github/github-flow
[gitflow]: https://nvie.com/posts/a-successful-git-branching-model/
[tbd]: https://trunkbaseddevelopment.com/
[gh-rulesets]: https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-rulesets/about-rulesets
[gh-rules]: https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-rulesets/available-rules-for-rulesets
[gh-protected]: https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-protected-branches/about-protected-branches
[gh-automerge]: https://docs.github.com/en/pull-requests/collaborating-with-pull-requests/incorporating-changes-from-a-pull-request/automatically-merging-a-pull-request
[gh-mq]: https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/configuring-pull-request-merges/managing-a-merge-queue
[gh-codeowners]: https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/customizing-your-repository/about-code-owners
[gh-contrib]: https://docs.github.com/en/communities/setting-up-your-project-for-healthy-contributions/setting-guidelines-for-repository-contributors
[gh-prtemplate]: https://docs.github.com/en/communities/using-templates-to-encourage-useful-issues-and-pull-requests/creating-a-pull-request-template-for-your-repository
[gh-issueforms]: https://docs.github.com/en/communities/using-templates-to-encourage-useful-issues-and-pull-requests/syntax-for-issue-forms
[gh-health]: https://docs.github.com/en/communities/setting-up-your-project-for-healthy-contributions/creating-a-default-community-health-file
[gh-labels]: https://docs.github.com/en/issues/using-labels-and-milestones-to-track-work/managing-labels
[gh-hardening]: https://docs.github.com/en/actions/security-for-github-actions/security-guides/security-hardening-for-github-actions
[gsl-pwn]: https://securitylab.github.com/resources/github-actions-preventing-pwn-requests/
[gh-fork-approval]: https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/enabling-features-for-your-repository/managing-github-actions-settings-for-a-repository
[gh-depreview]: https://docs.github.com/en/code-security/supply-chain-security/understanding-your-software-supply-chain/about-dependency-review
[vet]: https://mozilla.github.io/cargo-vet/
[gh-signing]: https://docs.github.com/en/authentication/managing-commit-signature-verification/about-commit-signature-verification
[gh-branches]: https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-branches-in-your-repository/viewing-branches-in-your-repository
[gh-autodelete]: https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/configuring-pull-request-merges/managing-the-automatic-deletion-of-branches
[gh-approve]: https://docs.github.com/en/pull-requests/collaborating-with-pull-requests/reviewing-changes-in-pull-requests/approving-a-pull-request-with-required-reviews
[gh-token]: https://docs.github.com/en/actions/concepts/security/github_token
[gh-push-prot]: https://docs.github.com/en/code-security/secret-scanning/introduction/about-push-protection
[gh-releases]: https://docs.github.com/en/repositories/releasing-projects-on-github/about-releases
[trojan]: https://trojansource.codes/
