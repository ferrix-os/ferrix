# Open branches: where to pick up

Every branch on GitHub that still holds work `main` does not have, as of
2026-10-01 evening (`main` 7afed6fdc), written at os-5d's wind-down, after
the product owner's and the certification consultant's; the 2026-10-04
wind-down's branches are in the sections after this introduction, and
*2026-10-05: refreshed* and *2026-10-05, evening: refreshed* say what of them
has landed since, and which branches origin no longer has; *2026-10-05,
wind-down* is the newest word. It is where the next session starts.
The *what* of each line of work is in its design document and in
`docs/BACKLOG.md`; this page says which branch carries it and which of a
family's branches to resume from.

**Unlanded** counts the branch's commits whose change is not on `main` under
any hash (`git cherry origin/main <branch>`), so a branch that was landed by
cherry-pick or rebase counts 0 and is not listed below. **Kind**:

- *work*: the branch to resume from;
- *history*: an earlier state of a line of work kept for reference (a
  pre-squash or pre-rebase copy, a `-hist` series); resume from its *work*
  sibling;
- *snapshot*: uncommitted work saved before a PC switch or a wind-down
  (`backup/…`, `wip/…`), one commit on whatever the worktree had; compare it
  with the family's *work* branch before using it;
- *never land*: diagnostics, profiling builds, scratch and negative-control
  copies. Read them, never land them.

To refresh this page: `git fetch --prune origin`, then for every
`origin/<branch>` not merged, `git cherry origin/main origin/<branch> | grep -c '^+'`.

Every landing still follows `docs/CONVENTIONS.md` and *What a landing runs* in
`docs/BACKLOG.md`: rebase onto `main`, `cargo xtask gate-rows --since main`
for the rows, gate on nazuna (`fleet/gate.sh`), the certification consultant
for the item, `land.sh`.

## 2026-10-05, wind-down

Written by po6-steward3 at the 2026-10-05 wind-down against `origin/main`
cf30aa08f, with `git cherry origin/main origin/<branch>`. Where it differs
from the two sections below, this one is newer.

**Landed since the evening section (e3e15bc7f), all on `main`:**

- the forecast and the reservation of steps 3a and 3b's ids (e41489fd7), and
  `open-branches` refreshed (e82805c14, f4cbaaa7f);
- FX-1012: `po6/g3`, a6116e822;
- batch 20261005T170400Z, as 3349682db: L13b (`l13b`, b909a18e7 through
  90dbef5cc), the stage 10 seam panic (`po6/seam`, 1e113bb31 and 3f0e56b2b),
  the Windows gateway's peek (`po6/gw2`, e321befc4) and `bench-ipc` made exact
  (`bench-exact`, f192d6baa as 3349682db);
- `land-n6`, NVIDIA N2 to N6's screen, as a84992dc5 (batch 20261005T190105Z,
  b136ac7d4 and the commits around it); `po6/land-n6` counts 0 unlanded now;
- the `--everything` fixes, `po6/everything-x11`, as 0d7520d82, 1752580c1 and
  cf30aa08f, on the customer's decision without a batch.

**In flight, not landed.** All of these are on origin as `po6/*`, pushed at
the wind-down without a force; the old origin names (`l13c`, `land-n6`,
`stage13-*`) keep their pre-rebase tips and are history.

| Branch | Tip | Based on | State |
|---|---|---|---|
| `po6/cgctl` | 69fe7b51c | 3349682db | the cgroup controllers (stage 13, 30 points); 23 controls fired, gates passed; owed: the consultant's look at the D1 fix and four new controls |
| `po6/cgctl-n6` | c4e2b8f9d | 3349682db (it carries `land-n6`) | the same work with coverage carried; no gate run yet; the one to rebase now that `land-n6` has landed |
| `po6/l13c` | aed1c3fa0 | 3349682db | init L13c, `SystemCallFilter=`; batch 20261005T184633Z failed it on `test-vfs --arch x86_64 --init ferrousli` (the cgroup applet's `rmdir`, "Resource busy"; `main` and `main` plus `land-n6` passed); next: rerun that gate on the branch alone, then rejoin |
| `po6/step2f` | df5411820 | 3349682db | the channel round trip's 2f: `check` passed, 4 controls fired, bench 2417 ns against `main`'s 2536 to 2566; the consultant: OK if C1 to C6, ledger 382; next: rebase, regenerate, carry coverage, join |
| `po6/step3` | 4a8b8dfee | 3349682db | steps 3a and 3b, WIP: `check` and the x86-64 KVM boot passed; owed: control c5, the TCG, AArch64 and ARMv7-A boots, `bench-ipc --alternate`, the consultant; one batch for both |
| `po6/step4-prep` | fe424cd81 | 527201573 | step 4's groundwork (the park protocol's loom model, `ferrix.fastpath`, the equivalence cases); waits for review |
| `po6/n5` | 511c1bdaf | e41489fd7 | N5, unprivileged mounting; parked by the customer; the consultant has not cleared it (ledger 378: B1 mounts detached before the rename check, B2 the namespace write-out under `preempt_disable`) |
| `po6/bwrap-user` | 09e9c9c74 | e41489fd7 | `test-bwrap` as uid 1000; parked with N5; the two together are re-sized from 5 to 16 points |

Still on the gate host, not on origin: `po6/seam-diag*` and `po5/red-diag`
(diagnostics, never land) and the worktrees and target directories the
handover lists for removal by exact name. `nvidia-n2` (N3b, half written, its
tip does not build) is the development branch for what follows `land-n6`.

## 2026-10-05, evening: refreshed

Refreshed by po6-steward2 on 2026-10-05 against `origin/main` e3e15bc7f,
after `git fetch --prune origin`, with `git cherry origin/main origin/<branch>`
for every branch the tables name. The *2026-10-05: refreshed* section below
is dated against 634c7ed0e; where it says "in progress", this section is the
newer word.

**Origin was cleaned on 2026-10-05.** 122 branches were deleted from GitHub
(landed, patch-equal, or pre-rebase copies of work that is on `main`); origin
holds 119 branches besides `main` now. Every deleted tip is in
`~/.local/share/ferrix/branch-archive-2026-10-05/origin-all.bundle` on the
gate host (its lists are `safe.txt` and `superseded.tsv`; the cleanup of
nazuna's worktrees and target dirs is in `nazuna-cleanup.log` there). So
many branches the tables name are no longer on origin. This page drops a row
only for a branch that is gone and whose work is on `main` (each of its
unlanded commits has its subject on `main`, or the cleanup's own lists call
it landed): 26 rows, those of `foot-shell`, `l13-init`, `np-land`,
`selfhost-matrix`, `selfhost-components`, `po-cpu-learnings`,
`stage13-s3-on-netns`, `s3-onmain`, `os-02/dead`, `os-02/unix-gc`,
`os-12/crng`, `os-12/curl`, `os-12/https`, `os-12/btop` to `btop4`,
`win-parity/gateway`, `default-wallpaper`, `chrome-groundwork`,
`cov-before-rebase4`, `fuzzel-window-pre-split`, `board-reset-3c-pre-rebase`,
`os-d1/armv7-compositor`, `pixel7-chromium` and `pixel7-vm-try`. A row that
names a branch origin no longer has stays when some of its commits are not
on `main` under any subject; read its tip from the bundle (`git fetch
<bundle> refs/remotes/origin/<branch>`). Those, as of this refresh, are
`fuzzel-window-prerebase`, `guest-frame-time-local`, `netns-presquash`, `os-12/ports`, `os-12/ports-curl-btop`, `os-35/ipc-lazytlb`, `os-35/ipc-lazytlb-571ea292`, `os-35/ipc-lazytlb-on-eeaa`, `os-35/ipc-lazytlb-pre-measure`, `os-35/ipc-lazytlb-wip`, `os-8d/stage20-before-rebase`, `os-ac/venus-wip`, `os-ipc/zircon-trip`, `s2-backup`, `s4-hist`, `s5-hist`, `s6-hist`, `s6-hist2`, `smallns-presquash`, `stage13-fdinfo`, `stage13-fdinfo-presquash`, `stage13-fdinfo-v2`, `stage13-fdinfo-v3`, `stage13-integ`, `stage13-np`, `stage13-s3`, `stage13-s3-onmain`, `stage13-s3-wip`, `stage13-seccomp`, `timens-presquash`, `timens-presquash2`, `timens-presquash3`, `unix-creds`, `vmo-map-wip`, `zinc`. The three IPC branches `step2f`, `bench-exact` and `step4-prep`
were never on GitHub (local on the gate host, as that section says) and are
unchanged.

**Landed on 2026-10-05 after 634c7ed0e**, all on `main`:

- po5-docs, po5-skill and po5-c1, the last as 6384c4861 (the verification
  audit's rows in `docs/BACKLOG.md`; `po5/c1` is an ancestor of `main`),
  with the two small fixes of the night section's item 6: FINDINGS' count
  (1797bc129) and the Pixel 7 handover (c19aeefcb), so `po5/docs` is gone;
- the gateway fix, `po5/gateway-retransmit`: 1d726d9ac (the retransmission
  timer out of the fast-retransmit test) and 39e520e31, an ancestor of
  `main`; the Windows job's red of the night section's item 5 is that, and
  d6849c86b records a second Windows gateway flake;
- s2-plan (`po5/s2-plan`): ce133294c and a3b240c6b, stage 20's S-2, every
  touched test builds its variants before the first boot;
- c1-n12 (`po5/c1-n12`): da133821e, f746a3e47, 82c12c721 and 904d1af7c,
  F-63 filed and closed, N10, N12 and N13 done;
- red = `po5/red-main` and `po5/sigterm`: the steal rule 4419e7b82 and its
  coverage d779e31f4, test-init's password and reader fixes ae5b2c8b8 and
  abe1b2583 (ae5b2c8b8 also carries xtask's non-UTF-8 reader fix, which is
  `po5/sigterm`'s ee2cd25f3), the diagnosis 6ad46f503, and the Backlog
  entries up to 0ece8826e. `po6/red` counts 0 unlanded; `po5/red-main` (2)
  and `po5/sigterm` (2) still count `+` because their commits landed under
  other hashes, history now;
- docs rounds: d6849c86b, fcc5f43e0 (the docs steward as a project agent)
  and e3e15bc7f.

**In progress now, not landed.** None of these is on origin except where it
says so; the others are branches in the gate host's repository, which `git
fetch --prune origin` does not show.

- `po6/g3`: FX-1012, the stage 10 G3 fix (G3 and R5 take their check byte
  in the receive path, out of the pump's reach). **Landed** as a6116e822
  after this list was written.
- `l13b` and `l13c` (session po6-l13): origin still has the pre-rebase tips
  (fac927209 with 5 unlanded, 91045526a with 10); the gate host's local
  branches are rebased onto `main` (f08289ef4 with 4, 3f0501d1e with 7).
- `po6/seam` (d9acd2ef8, 2 unlanded, on the gate host): the stage 10 seam
  panic. Its `po6/seam-diag`, `po6/seam-diag-fix` and `po6/seam-diag3` are
  diagnostics beside it.
- `po6/gw2`: the Windows gateway's refused-connect flake (the second flake
  d6849c86b records). No branch by that name exists on the gate host yet.
- `land-n6`, NVIDIA N2 to N6: `po5/land-n6` (47b0a2294, 7 unlanded, on the
  gate host, not on origin) is the resume point, ready; it waits for the G3
  fix. On origin, `land-n6` (3d6621a64, 1) and `tonight/land-n6` (2e936ad65,
  6) are its earlier states. `nvidia-n1c` (1 unlanded) is on origin and was
  not on this page.
- `po5/red-diag` (6 unlanded, K2 diagnostics) is on the gate host, not for
  landing.

The next section's other in-progress items are settled as above (po5-gw,
po5-red, the two held-back fixes). `nvidia-n2` still counts 21 unlanded, and
`stage13-netns` counts 5 as a pre-rebase copy of 22384874f.

## 2026-10-05: refreshed

Refreshed by po5-docs on 2026-10-05 against `origin/main` 634c7ed0e, after
`git fetch --prune origin`, with `git cherry origin/main origin/<branch>` for
every branch below.

**Landed on the night of 2026-10-04**, each with 0 unlanded on its
`tonight/` branch (or merged whole):

- S3, `tonight/stage13-s3-on-netns`: 248799bdd through 3e8b759d3;
- NP, `tonight/np-land`: 7e9a2806f, 4538d75b6 and a18b3fd33 (the one `+` is
  explained below);
- L13a, `tonight/l13-init`: 7121c885e through 792858179;
- `foot-shell` and `tonight/foot-shell`: f55e8ab28;
- `selfhost-matrix` and `tonight/selfhost-matrix`: tip 77783565a;
- `selfhost-components`: cb872a732;
- `po-skill`: 8eac36ec1 and bc0a1b847; `po-cpu-learnings` (67efb9fb1) and
  `docs-sizing` (634c7ed0e) are merged.

The plain `stage13-s3-on-netns` (2), `l13-init` (2), `np-land` (1) and
`selfhost-matrix` (2) still count `+` commits, but each is a pre-rebase copy
of a commit on `main` with the same subject (cd1f71304 is 248799bdd,
1373cf7ca is 7e9a2806f, 21d67b095 is 792858179, e8b57ed4f is 8a128e024, and
so on): history now, nothing to land.

**`tonight/np-land`'s single `+`, 240fd2ccc, is a pre-rebase copy of
7e9a2806f.** Both carry the same message and author date (2026-10-04
15:59); 240fd2ccc was committed on 67efb9fb1, before S3, and 7e9a2806f is
the same change rebased onto S3 (parent 3e8b759d3). Their changes are
identical: `git diff -U0` of each against its parent, with the `index` and
`@@` lines dropped, is the same for `src/` and for the whole commit. Under
the default three lines of context they differ only in context S3 added
(the seccomp row of the stage 13 page, `first_seccomp` in
`syscall/process.rs`), which is why the patch ids differ and `git cherry`
counts it. Nothing of NP is unlanded.

**In progress on 2026-10-05:**

- `tonight/land-n6` (2e936ad65, 6 unlanded): carried by session po5-n6.
- CI's `Test (windows-latest)` red on `main` (night item 5): session po5-gw.
- `main`'s flaky gates (the two red gates of the night section, the
  console-revoke reader under load and the QEMU ended from outside): session
  po5-red.
- The two held-back fixes (night item 6): branch `po5/docs`, with the
  consultant's OK for the FINDINGS count (ledger 356-357).

Still work, unchanged: `l13b`, `l13c` (each to rebase now that S3 and L13a
are on `main`) and `nvidia-n2`.

## Night of 2026-10-04: start here

Written by os-7c at the customer's wind-down at 22:00 on 2026-10-04, after a
landing round from 19:50 that a held gate pool (an AOSP build on nazuna,
20:37-20:52) and a power cut on the Windows PC (about 21:15; nazuna is on a
UPS) cut short. `main` was 50518ac1d at 22:00. Tonight landed `selfhost-components`
as cb872a732 (batch 20261004T185433Z), which fixes CI's self-hosting job, and
`selfhost-matrix` (below, item 2; its tip on `main` is 77783565a); the
sections further down say so since 2026-10-05.
Every branch below is on GitHub under `tonight/<name>`, rebased onto
67efb9fb1 unless it says otherwise, and on nazuna as `os7c-tonight/<name>`;
each has its worktree under `.claude/worktrees/tonight-<name>`. The
`tonight/*` branches supersede the branches named in the next section.

**Main's two red gates, both read as the gate pool, not code.** In
progress 2026-10-05: session po5-red. Batch
20261004T190141Z ended MAIN-RED on cb872a732:

- `test-shell --arch all --init ferrousli`: QEMU ended by signal 15 7.0 s
  into the x86_64 boot (b0-1), six times. Not the host: xtask sent it, after
  a stage 10 panic's echoed 0xF5 ended its reader (po5-sig, 2026-10-05; the
  row in `docs/BACKLOG.md`'s *Red on `main`*, xtask half on `po6/red`).
- `test-init --arch all`: aarch64 only, "the revoke stage's reader was not
  waiting (state R)" (b0-2), a timing check of console-revoke's under load.
  The same tip's own `test-init --arch aarch64` passed. Owner: console-revoke's
  session (ferrix-da); make the wait robust or file it as a flake with its row.
- `test-init` in a gate slot also fails "no sshdt built for x86_64" unless the
  slot has `cargo xtask build-apps --arch x86_64 --app sshdt`; slots 3 and 4
  have it since tonight, slot 2 has it, check 1 and 5. The batch should build
  what its gates need (a row for the gate pool's owner).

In this order:

1. **LANDED: all four** on `main` as f55e8ab28 and the commits before it
   (S3 248799bdd, NP 7e9a2806f, L13a 1e1f2543a, foot-shell f55e8ab28). What
   follows is as written before they landed. **`tonight/stage13-s3-on-netns`** (8bf380332), **`tonight/np-land`**
   (3b04624ba), **`tonight/l13-init`** (c2e457f6d) and **`tonight/foot-shell`**
   (ec976b389): each passed everything it owes before the batch (check, its
   controls FIRED, its consultant: s3 OK IF ledger 347, np OK IF 346, l13 and
   foot outside the item). Their batch was MAIN-RED, and its tip also failed
   `test-init --arch all` on aarch64 (`login plain` did not log in, /dev/tty
   checks, revoke reader never started; batch-20261004T190141Z-5) while the
   tip's per-arch aarch64 line passed. Before rejoining, re-run `test-init
   --arch aarch64` on that tip (99c5166cd) twice: if it fails, bisect the four
   on aarch64 (np-land touches /proc and `may_access`, the likeliest). Then
   rebase onto main and join again as one batch.
2. **`tonight/selfhost-matrix`: LANDED** on `main` (tip 77783565a, after the
   batch below; check the product owner's record for the controls' and the
   transcript comparison's verdicts). Before it landed, batch 20261004T191902Z passed
   16 of 17 gates on its tip 43d205bed; the 17th, `test-init`, failed only
   for "no sshdt built for x86_64" in its slot. Re-run that gate in a slot
   with sshdt, then the transcript comparison its consultant asks (OK IF,
   ledger 345), then land. selfhost-components merges cleanly with it.
3. **In progress 2026-10-05, session po5-n6.** **`tonight/land-n6`** (aac7eef62): batch 20261004T175923Z FAILED it with
   FX-0871 at every boot (exec loading read 65 of 18437 pages through a
   32-page read-ahead on the writable segment's tail page). The customer
   chose the fix: fill that page alone. 7db4a95f6 does that (consultant OK
   IF, ledger 348, H1-H4) but failed `check` and the KVM boot; aac7eef62
   then changes what the stage 8 check expects of that page, which is a
   check change and needs the consultant before anything else. `check`
   passed on aac7eef62; the boot, the control and the batch (~/n6-gates.txt,
   11 rows) are owed.
4. **LANDED 2026-10-04** as 8eac36ec1 and bc0a1b847. **`po-skill`** (this page's branch): the product owner's role as the
   skill `.claude/skills/product-owner/SKILL.md` and the agent
   `.claude/agents/product-owner.md`. Docs only, so it owes `check`; land it
   first tomorrow so the next PO can load it. Owed beside it: a dated
   *Decisions* entry in `docs/BACKLOG.md` for the customer's rule that each
   session briefs its own certification consultant (2026-10-04), and
   AGENTS.md's consultant section, which still describes one standing seat.
5. **In progress 2026-10-05, session po5-gw.** **CI's `Test (windows-latest)` is red on `main`** since at least
   67efb9fb1: `gateway::tests::a_lost_segment_is_sent_again_alone` fails on
   Windows only, with different numbers each run (tests.rs:1096, run
   37229234877, and 1105). Timing-dependent; 900c2e8c6 did not cover it.
   Next: reproduce on the Windows PC (50+ runs, also under load), find
   whether the gateway or the test is at fault, fix the gateway if it is
   (an xtask change: the image row on nazuna). Owner: open (the gateway's);
   the BACKLOG's *Red on `main`* has the row. Nobody works on it tonight.
6. **Two small fixes held back from the sizing landing** (branch
   `docs-sizing`, commit 39394d02b has both): `docs/certification/FINDINGS.md`'s
   severity table counts 8 Moderate open findings where 7 are (F-57 closed
   2026-10-03), which is certification evidence and lands with a
   consultant's look; and `src/boot/vendor/google/pixel7/HANDOVER.md` l. 312
   still says the USB driver is "Not started" though it landed 2026-09-26,
   which is Markdown under `src/`, outside the docs-only rule. Land each with
   its own row. Both are on branch `po5/docs` since 2026-10-05.

Leftovers on nazuna, to delete by exact name: `~/target-os7c-land-n6`,
`~/Documents/projects/os/ferrix/target-os7c-l13-init`,
`~/.local/share/ferrix/target-os7c-selfhost-matrix`, and the worktrees
`.claude/worktrees/os7c-n6`, `os7c-l13-init` and `os7c-shm` in
`~/Documents/projects/os/ferrix`. On 2026-10-05 `~/target-os7c-land-n6` was
removed (`nazuna-cleanup.log` in the branch archive), and on this refresh
`target-os7c-l13-init`, `target-os7c-selfhost-matrix` and the `os7c-shm`
worktree are not there either; `os7c-n6` and `os7c-l13-init` are still listed
as worktrees. The open queue's entries set aside during
the hold are in `~/.local/share/ferrix/fleet/batch/po-aside-2026-10-04/`.

## Wind-down 2026-10-04: start here

Written by the product owner (ferrix-d7) at the customer's wind-down on
2026-10-04, from each session's own entry; `main` was 900c2e8c6, pushed. Every
branch below is on GitHub. Landed that day, through `fleet/batch.sh`
(`AGENTS.md`, *Batching full runs*): console-revoke and `stage13-netns`
(22384874f), and the apps pin and the gateway test's flake fix (900c2e8c6).

In this order:

Since 2026-10-05: every branch of this list has landed but `land-n6`
(session po5-n6), `l13b`, `l13c` and `nvidia-n2`.

1. **LANDED** (cb872a732). **`selfhost-components`** first: `main`'s CI job "rustc on Ferrix, and
   Ferrix built on Ferrix" is red until it lands (since 2263225e1, the
   components split). The Windows job's red was a flake, fixed in 900c2e8c6.
2. The batch that was running at the wind-down, 20261004T135748Z
   (`stage13-s3-on-netns`, `l13-init`, `selfhost-components` and the
   product owner's `po-cpu-learnings`), was stopped before its verdict, so
   it is NOT DECIDED. Its full run failed only `test-selfhost` (red on
   `main` until `selfhost-components` lands) and `test-shell --arch all
   --init ferrousli`, whose x86_64 QEMU ended from outside 7.1 s into the
   boot; every other gate passed. Each entry joins again once.
3. Then `land-n6`, `foot-shell`, `np-land` and `selfhost-matrix`, each as its
   bullet says, then the work branches (`l13b`, `l13c`, `nvidia-n2`).

Gate pool notes, for whoever runs it:

- It has five slots since 2026-10-04 (the customer); with three, the CPU sat
  70-80% idle while runs queued (the `product-owner` skill, *Busy slots on an idle
  processor*).
- Something on the host ended QEMUs 7-8 s into a boot three times on
  2026-10-04 (ferrix-da twice, the batch once), with no OOM in the kernel log
  and not xtask's own orphan killer. Not found; when it happens, take `ps -eo
  pid,ppid,lstart,args` at once.
- `batch.sh` gaps, rows for the product owner: `batch.sh wait` restarts the
  runner even when the fleet is told to hold; a gate red on `main` that an
  entry of the batch fixes makes the search for the failing entry end in
  MAIN-RED; the next batch must not close before the last PASSED stack has
  been landed or based on (fixed for new runners: `passed-tip`).
- Another session on the host (an AOSP build) asked at the wind-down that
  nothing heavy start until it says so; check with the customer before the
  first gate.

### Stage 13 and init's L13 (ferrix-21; handover `~/.local/share/ferrix/l13-coord/HANDOVER.md`)

- `stage13-s3-on-netns` (0527dd365; **landed 2026-10-04** from
  `tonight/stage13-s3-on-netns`, 248799bdd through 3e8b759d3; what follows is
  as written before): S3 on main 22384874f. Passed on
  6c539b276 before the rebase onto netns: 8 rows and all 19 controls
  (`l13s3f-*`); consultant OK at ledger line 336, B1/B2 closed at 327. On
  0527dd365: k7, k13 and k17 FIRED (`l13s3n2-*`); the full rows were in
  batch 20261004T135748Z . The other 16 controls carry under the line-321
  rule; evidence `git range-diff 65a33486c..6c539b276 22384874f..0527dd365
  -- src tools/common/data`, saved at
  `~/.local/share/ferrix/logs/s3-range-diff.txt`. Next: send the batch
  verdict with the range-diff to the consultant for the final OK, then
  update SECCOMP.md §12 and the roadmap. Trap: a boot check's user tasks
  must be listed (`check::spawn_in`), or main's pending-work word panics
  FX-0520. `stage13-s3`, `stage13-s3-onmain` and `stage13-s3-rebase` are
  history.
- `l13-init` (21d67b095; **landed 2026-10-04** from `tonight/l13-init`,
  7121c885e through 792858179), L13a: NoNewPrivileges=, PrivateTmp= and
  ProtectSystem= (INIT.md §4.5). check and test-init on all three arches
  passed on 5748c67fb (`l13init-*`); rebased onto 22384874f; in batch
  20261004T135748Z. Outside the item (ledger line 323). Trap: docs/generated
  conflicts on every rebase; take main's copy, then rerun gen-arch-doc.py.
- `l13b` (fac927209), PrivateNetwork= (a network namespace with lo up), on
  l13-init. A local x86 test-init passed and the netns and loup controls
  FIRED; not pool-gated. Next: rebase onto main (l13-init has landed), then a
  batch with `--profile none`, gate `check` and `test-init --arch all`.
- `l13c` (91045526a), SystemCallFilter=, SystemCallErrorNumber= and
  SystemCallArchitectures= as per-ABI BPF, on S3 plus l13b. A local x86
  test-init passed; its negative controls were cut off by the wind-down
  (script `~/.local/share/ferrix/logs/l13c-ctl.py`). Next: run them, rebase
  after S3 and l13b land, then a batch with `check` and `test-init --arch
  all`.

### Authentication phase 2 (ferrix-da)

- `np-land` (403b05626; **landed 2026-10-04** from `tonight/np-land` as
  7e9a2806f, 4538d75b6 and a18b3fd33; what follows is as written before;
  handover
  `docs/handover/2026-10-04-np.md` on the branch): NP (`/proc` by
  `ptrace_may_access` with dumpability, and `/proc/<pid>/fdinfo`) landing as
  AUTH P2.1. It is `stage13-fdinfo` rebased onto main 22384874f and squashed
  into 1373cf7ca, so every commit builds; it supersedes `stage13-fdinfo`.
  Since the rebase onto netns, `ns/net`'s readlink and open
  (`net::netns_file::may_open`) ask `credentials::may_access` too, and
  procacc opens `ns/net`. Passed: test-boot x86_64 (procacc 309 calls, 155
  refusals; netns 1103/19); coverage carried, 0 anchors dropped; `check` on
  31faa9f0e (owed on 1373cf7ca). Controls FIRED: c01, c02, c04, c05, c06 on
  31faa9f0e; c04 and c06 again on 1373cf7ca; c03 fired on the allowed-side
  message, which ledger line 342 accepts; c02, c03 and c12 were left running
  on 1373cf7ca (`logs/queue/np3-*.log`). Owed: the controls not yet FIRED on
  the landing hash (c01-c03, c05, c07-c13; c13 puts netns `may_open` back to
  ids only), `check`, then `batch.sh join` with the full profile plus
  `test-init --arch all`, `test-shell` (busybox and zinc) and `test-vfs`
  (ferrousli). Consultant OK IF, ledger 226, 338, 342. Next: rebase onto
  main, `check`, the 13 controls (lines in the handover), join. Traps: a
  union merge of `catalog.rs` can leave a `};` inside `ALL`; netns's
  `Place::Namespace(pid, Net)` arm must stay inside the guarded match.
- `console-revoke` **landed 2026-10-04** as 0f94a6d1a in batch
  20261004T133714Z (main 22384874f); consultant lines 329, 339, 341 met.

### Stage 20: Ferrix built on Ferrix (ferrix-d4)

- `selfhost-matrix` (e8b57ed4f; **landed 2026-10-04**, tip 77783565a, the
  owed list below is as written before it): pid 1's inputs (init program, `sh -c`
  script, command list) move from the kernel into the initramfs under
  `.ferrix/init/`, so one kernel serves every test (stage 20's plan: 66
  kernel builds to 6, 271 builds to 188; about 2 s a gate on nazuna), plus
  the stage 20 harness outside the item (`FERRIX_BUILDS=plan:`, the 57-row
  `selfhost-matrix.sh plan|record|replay`, script apps recorded,
  test-selfhost --plan vendoring). Consultant OK IF, ledger lines 328,
  331-333, 340, 343. Passed: host tests, kernel clippy x3, traceability,
  item boundary, coverage carried; test-boot, test-init, test-shell (musl)
  and test-vfs on x86_64 by hand; controls b, c1-c7 and e FIRED on
  5e2eca4b2. Owed: (1) `cargo xtask check` on the head (stopped at the
  wind-down), (2) every control in
  `~/.local/share/ferrix/logs/linit-controls.md` again on the landing hash,
  with the spec's re-run notes for a, g, e-host, f and f2, (3) `batch.sh
  join ... --gate ~/.local/share/ferrix/selfhost-matrix/batch-gate.txt`,
  then the one-line report to the PO (all controls FIRED, the four boots'
  transcripts equal main's apart from the `inputs` line and sizes). Traps:
  5e2eca4b2 failed `cargo fmt --check` in src/lib/fs/vfs/src/tests.rs (fixed
  0eda22d00), which is why e-host and g did not fire; `test-init` in a gate
  slot needs `build-apps --arch x86_64 --app sshdt` there first (pin with
  GATE_SLOT); the generated architecture files conflict on every rebase
  (take either side, run tools/common/gen/gen-arch-doc.py); carry-coverage
  after a commit that wrote the coverage files needs `--from HEAD`;
  boot-21b's H.BOOT.10-14 reconcile with H.BOOT.15 at whichever lands second
  (ledger 333). Next after landing: tests build every variant before their
  first boot (approved), then the weekly CI job. Handover:
  `docs/handover/2026-10-04-stage20-selfhost.md` on the branch.

### Components, apps and main's red CI (ferrix-db)

- `selfhost-components` (1fcbe409e, 1 commit; **landed 2026-10-04** as
  cb872a732): fixes main's red "rustc on
  Ferrix, and Ferrix built on Ferrix" CI job, broken by the components split
  (2263225e1, landed by components-flip). test-selfhost now copies each
  component checkout's tracked files into the volume, and
  `components::ensure()` skips a tree with no `.git`, so the guest never
  clones. Passed: `test-selfhost --accel kvm` locally, clippy and fmt. It
  touches only `tools/common/xtask`, outside the item, but that is the image
  row: it owes one full-profile batch with its test-selfhost line (batch
  20261004T135748Z was stopped at the wind-down, NOT DECIDED). **Fix this
  first: main's CI is red until it lands.** Next: rebase onto main,
  `batch.sh join <session> selfhost-components selfhost-components --gate
  <file: test-selfhost --accel kvm --timeout 1800>`.
- `foot-shell` (3e5fd3c02, 1 commit; **landed 2026-10-04** as f55e8ab28):
  test-foot carries zinc as `/bin/sh`
  for its foot-check script; before this, the keyboard check from def906ba2
  never ran ("failed to execute: No such file or directory"). Passed
  locally: `foot-typed: ok`, and the control with XKB_DIRECTORIES emptied
  fails with "the keys pressed never reached its program". Owed: a
  full-profile batch. Next: rebase onto main, `batch.sh join` with the full
  profile. Trap: test-foot can't run in the gate slots until a slot has
  `cargo xtask build-apps --arch x86_64 --app foot` (a row for the test-time
  owner); until then its evidence is from a session's own target.
- Red, not owned: test-badapple armv7a's window-on-the-desktop boot, red
  since at least 594d69146: "holding frame 359" while the dump shows an
  earlier frame (62057 of 196608 pixels wrong), hyprix drawing in software
  on armv7a. The window path was only gated on x86_64.

### NVIDIA on the RTX 3060 (ferrix-74)

- `land-n6` (3d6621a64, one squashed commit; carried since 2026-10-05 by
  session po5-n6 from `tonight/land-n6`, 2e936ad65): resume the NVIDIA landing
  here. Vulkan, NVKMS and nvrm as the display core's copying driver
  (displayctl v8 copies flag, `Refusal::Copies`), PIN_CONTIGUOUS, devfs
  inotify, init `.device` units (hyprix.service
  `Requires=dev-dri-card0.device`) and `run-compositor --nvidia`: Chrome
  renders WebGL on the RTX 3060, shown on the 3060's own monitor. Consultant
  land-ahead verdict, ledger 318, L1-L6: L1 and L3-L6 are in the commit, L2
  is the gate rows. `cargo xtask check` PASSED on 3d6621a64; a batch DROPPED
  it on docs/generated conflicts. Next: rebase onto main, take main's
  `docs/generated/*`, run `cargo xtask model-doc`, carry-coverage only if
  main touched `src/kernel`, amend, then `batch.sh join … land-n6` with the
  full profile plus `test-compositor --arch x86_64 --accel kvm`,
  `test-compositor --arch aarch64`, `test-nvrm --arch x86_64` and
  `test-nvrm-link --arch x86_64`. Trap: docs/generated conflicts on every
  rebase; regenerate, never merge by hand. Owed after landing (BACKLOG
  O1-O6): ledgers 293 D7/D8/D10, 310 E1-E6/devfs D2-D5/C3/C4/C10/K1-K7, the
  294/300 rows; F-61 open.
- `nvidia-n2` (dbb23fd6f): continue NVIDIA work here; handover
  `docs/handover/2026-10-03-nvidia-n6.md` with its 2026-10-04 update.
  Everything in land-n6, plus `FERRIX_NVIDIA_INPUT` evdev keyboard/mouse
  passthrough (worked on the TV), plus N3b in progress: Chrome GPU
  compositing over dmabuf for 60 fps (design OK IF, ledger 316 B1-B14).
  Done: nvrm's nvidia-drm subset `os/glue/drm.c` and the nvos bindings.
  Half-written: the kernel render node, dmabuf object and native calls
  0x1060/0x1061, and hyprix `zwp_linux_dmabuf_v1`. Next: finish the kernel
  and hyprix halves, boot `run-compositor --nvidia` with the TV on and read
  `webgl-fps`; then the customer's order, a hardware cursor through NVKMS,
  then measuring page loads (network against rendering). Trap: HEAD
  (36cc30354 onward) does NOT build; the last building state is d8c980c9b
  plus 76301d0f4 and b1b35a3db.

| Branch | Last commit | Unlanded | Kind | Tip |
|---|---|---:|---|---|
| `l13b` | 2026-10-04 | 5 | work: this is the pre-rebase tip; resume from the gate host's local `l13b` (f08289ef4, 4 unlanded, rebased onto `main`), po6-l13 | INIT.md: L13b, PrivateNetwork=, as built |
| `l13c` | 2026-10-04 | 10 | work: this is the pre-rebase tip; resume from the gate host's local `l13c` (3f0501d1e, 7 unlanded, rebased onto `main`), po6-l13 | test-init: filter checks for SystemCallFilter= and its two keys |
| `land-n6` | 2026-10-04 | 1 | history: resume from `po5/land-n6` on the gate host (47b0a2294, 7 unlanded, not on origin), po5-n6; ready, waiting for the G3 fix | NVIDIA on the RTX 3060: Vulkan, NVKMS and the card's own monitor; Chrome renders WebGL there |
| `tonight/land-n6` | 2026-10-04 | 6 | history: land-n6's earlier state on origin; `po5/land-n6` is later | docs: regenerate the model and traceability after the rebase onto f55e8ab28 |
| `nvidia-n1c` | 2026-10-03 | 1 | work: not named before this refresh; NVIDIA N1c | WIP (N1c, wind-down): nvrm's OS layer, ferrix-nvos and the kept C |
| `nvidia-n2` | 2026-10-04 | 21 | work | Handover update: land-n6 dropped by a docs/generated conflict, input passthrough works, the customer's performance order |

## Stage 13: namespaces, cgroups, seccomp

os-7c's [stage 13 handover](stage-13-handover.md), landed the same evening,
is the fuller account of these branches and what each owes; read it first.
The stage's exit is the container test. State on 2026-10-01: N4, S1, S2,
smallns and pidns landed; the rest are chains of os-7c's agents. Resume from
the plain `stage13-<part>` branch of each; the `-presquash`, `-hist`, `-v2`,
`-v3`, `-onmain` and `-dbg` branches are its earlier states.

- `stage13-cgctl`: the consultant accepted its fixes; it waits for the gate
  and 18 controls. It holds L.object.106-112, L.sched.3-4 and H.QUOTA.10-12,
  and its landing deletes its entry in
  `tools/common/data/requirement-reservations.json`.
- `stage13-s3`: resumed as `stage13-s3-on-netns`, which **landed
  2026-10-04** (248799bdd through 3e8b759d3); `stage13-s3`, `-onmain` and
  `-rebase` are history. Then `stage13-s4` (design OK with conditions: an L row, a VA row,
  native calls fail closed, a restart-code case), `stage13-s5`, `stage13-s6`.
  `stage13-s3-wip` does not build.
- `stage13-netns` **landed 2026-10-04** in main 22384874f (batch
  20261004T133714Z), gated on 2428b0d95 (`l13ns-*`; ledger line 333). Next is
  `stage13-timens`, which must rebase onto it.
- `stage13-fdinfo` (NP) is superseded by `np-land`, which **landed
  2026-10-04** as 7e9a2806f; then `stage13-n5`, then `stage13-bwrap-user`.
- `stage13-container`: the exit criterion as one program, never run.
- `stage13-timens`'s worktree held its work staged with a conflict in
  `panic/catalog.rs` (FX-0907 beside FX-0910): take both, then regenerate
  `docs/generated/PANICS.md` and the coverage JSONs that read it.

Every namespace and seccomp branch got the same blocker once:
`launch::load_native` must give a native child the creator's whole namespace
set, pid namespace, seccomp chain and `no_new_privs`. netns and S3 (both
landed) carry it. The consultant's ledger
is `~/.local/share/ferrix/cert-consultant/reviews.md` on nazuna.

| Branch | Last commit | Unlanded | Kind | Tip |
|---|---|---:|---|---|
| `timens-presquash3` | 2026-10-01 | 8 | history | wip timens: native-child VA row, roadmap |
| `timens-presquash2` | 2026-10-01 | 2 | history | wip timens on netns |
| `timens-presquash` | 2026-10-01 | 29 | history | BACKLOG: the time namespace's differences from Linux |
| `stage13-netns` | 2026-10-01 | 5 | history: landed 2026-10-04 (22384874f), pre-rebase copy | Roadmap: network namespaces' state at the wind-down |
| `stage13-timens` | 2026-10-01 | 6 | work | Roadmap: the time namespace's state at the wind-down |
| `stage13-s6` | 2026-10-01 | 12 | work | WIP: wind-down state of stage13-s6 |
| `stage13-s5` | 2026-10-01 | 10 | work | WIP: wind-down state of stage13-s5 |
| `stage13-s4` | 2026-10-01 | 7 | work | WIP: wind-down state of stage13-s4 |
| `stage13-s3-wip` | 2026-10-01 | 4 | history | WIP, does not build: S3's filters, half written when the customer asked to stop |
| `stage13-s3-onmain` | 2026-10-01 | 7 | history | WIP: wind-down state of s3-onmain |
| `stage13-s3` | 2026-10-01 | 7 | history | WIP: wind-down state of stage13-s3 |
| `stage13-n5` | 2026-10-01 | 15 | work | Roadmap: where N5 stands at the wind-down |
| `stage13-fdinfo-v3` | 2026-10-01 | 6 | history | NP: the small-namespace check's people say they are dumpable, as bubblewrap does |
| `stage13-fdinfo-v2` | 2026-10-01 | 4 | history | NP: the dumpable test applies on the capability path, mountinfo is Linux's, the check look |
| `stage13-fdinfo-presquash` | 2026-10-01 | 12 | history | NP: clippy, and the procacc line names what it reads |
| `stage13-fdinfo` | 2026-10-01 | 6 | history | Roadmap: where NP stands at the wind-down |
| `stage13-container` | 2026-10-01 | 17 | work | WIP: test-container, stage 13's exit criterion as one program, never run |
| `stage13-cgctl` | 2026-10-01 | 19 | work | WIP: record where stage13-cgctl stands at the wind-down |
| `stage13-bwrap-user` | 2026-10-01 | 16 | work | Roadmap: where test-bwrap as uid 1000 stands at the wind-down |
| `smallns-presquash` | 2026-10-01 | 13 | history | Write down the consultant's smallns findings: controls fired, rows restated, differences f |
| `s6-hist2` | 2026-10-01 | 30 | history | WIP S6: docs |
| `s6-hist` | 2026-10-01 | 22 | history | WIP S6: clippy |
| `s5-hist` | 2026-10-01 | 17 | history | WIP S5: docs |
| `s4-hist` | 2026-10-01 | 13 | history | WIP S4: docs say what Linux does with a blocked SIGSYS |
| `s2-backup` | 2026-10-01 | 11 | history | S2: record the review, the deviations, the gates and the controls |
| `pidns-dbg` | 2026-10-01 | 12 | never land | DEBUG3 |
| `netns-presquash` | 2026-10-01 | 21 | history | The small namespaces' flag check no longer expects network namespaces to be refused |
| `cgctl-dbg` | 2026-10-01 | 11 | never land | DEBUG kmem |
| `backup/2026-10-01/wip-timens` | 2026-10-01 | 6 | snapshot | Backup of uncommitted work in timens (stage13-timens) before a PC switch, 2026-10-01 |
| `backup/2026-10-01/wip-n5` | 2026-10-01 | 15 | snapshot | Backup of uncommitted work in n5 (stage13-n5) before a PC switch, 2026-10-01 |
| `backup/2026-10-01/stage13-n5` | 2026-10-01 | 14 | snapshot | N5: a plain remount asks for privilege over the filesystem's owner; the bottom's flags loc |
| `backup/2026-10-01/stage13-fdinfo` | 2026-10-01 | 5 | snapshot | NP: the stat address-field residual in the vulnerability analysis; the proc_fd_link row na |
| `stage13-seccomp` | 2026-09-30 | 18 | history | seccomp: the design and records |
| `stage13-np` | 2026-09-30 | 18 | history | NP: the check lists the fd directory, which is what is guarded |
| `stage13-integ` | 2026-09-30 | 17 | history | N4: nested 32 deep, EUSERS, and the errnos CLONE_NEWUSER may answer (SECCOMP R3, R5) |

## The channel round trip (IPC fast path)

The plan is `docs/OPAQUE-KERNEL.md` §9.5 to §9.8, and §9.9 says where it
stood at the 2026-10-03 wind-down; the session's account is
[the IPC handover](../handover/2026-10-03-ipc.md). The target is seL4's own
figure or better (440 ns a round trip on nazuna, the customer, 2026-10-02).
Step 1, 2a to 2e and F-60's fix are on `main`; these branches hold the rest.
Each lands only with the certification consultant's OK. **These three are
local branches on nazuna, not yet on GitHub**: pushing them is the
customer's call.

- `step2f` (bc256748a, on the `step2f-ids` reservation, now on `main`): 2f,
  no global or locked writes in the switch. **WIP, ungated, not reviewed.**
  Built for x86-64 only; the weight arithmetic's host test passes. Owed, in
  order: the rows L.sched.50-53 and L.object.160-161 (until they exist,
  `check` fails on their `Verifies:` tags); the condition-6 table re-read
  against the code, with F-60's `answer_leaving` as a remote reader of
  `LAST_DOMAIN`; a stage-5 case that a running processor never reads as idle;
  five controls (the idle clear skipped, `take_resched` on the wrong
  processor, `LAST_DOMAIN` stored after the compare, the host test's division
  left at `u32`, and the new `regroup` loom model's control, which has not
  run); SPECULATION.md §3, coverage, the full gate and `bench-ipc`; then the
  consultant. About 3 to 4 hours. Worktree `.claude/worktrees/ipc-step1`
  (warm target dir); logs `~/.local/share/ferrix/logs/step2a/`.
- `bench-exact` (81409fd7a): step 0's exact `bench-ipc`, a fenced counter,
  p50 from sorted samples, one pinned processor and `--alternate <ref>`.
  Outside the item. Its gate was not confirmed at the wind-down: run a
  full `cargo xtask check` on its rebased head before landing it. Land it
  first, since today's p50 moves in 233 ns steps and hides 2a's, 2c's and
  2e's savings.
- `step4-prep` (fe424cd81, three commits on the `step4-ids` reservation,
  now on `main`): step 4's groundwork. `ferrix.fastpath` (off by default,
  printed at stage 9, L.x86_64.150); the `loom` model of the park protocol
  (§9.7 condition 9, three controls); `ipc-equiv` and `cargo xtask
  test-ipc-equiv`, passing on the general path, with the cases that need
  threads, signals, affinity or 3a printed as owed. No fast-path code. Not
  reviewed; §9.7 on the branch says what it holds.
- Not started: 3a (the vector-state contract) and 3b (FS and GS kept in the
  task, the write-skip dropped), with the consultant's conditions 7 and 8 in
  §9.8; step 4's fast path itself, which needs 2f and 3a; step 5; step 4b.

Older branches of this work: `os-ipc/zircon-trip` is landed in substance (its
six commits went in as step 1), and `os-ipc/prof` and `os-ipc/prof2` are
timing builds that never land. The `os-35/ipc-*` branches below are os-35's
lazy TLB, ring B and sync wake; step 1 took the sync wake, and the lazy TLB
still needs its own landing.

| Branch | Last commit | Unlanded | Kind | Tip |
|---|---|---:|---|---|
| `step2f` | 2026-10-03 | 1 | work | WIP: 2f, no global or locked writes in the switch (wind-down, ungated) |
| `bench-exact` | 2026-10-03 | 1 | work | bench-ipc made exact: fenced counter, sorted samples, one pinned processor, alternation |
| `step4-prep` | 2026-10-03 | 3 | work | Say where step 4 stands (OPAQUE-KERNEL.md §9.7) |
| `os-ipc/zircon-trip` | 2026-10-01 | 10 | history | WIP: save a task's vector state with XSAVEOPT at a switch, where the processor has it |
| `os-ipc/prof2` | 2026-10-01 | 10 | never land | PROFILE: user-state sub-spans |
| `os-ipc/prof` | 2026-10-01 | 5 | never land | PROFILE, not for landing: spans of a round trip |
| `os-35/ipc-wake` | 2026-10-01 | 3 | work | WIP: poll before an idle halt, and interrupt one core with a targeted SGI (os-35 part C) |
| `os-35/ipc-ring-land` | 2026-10-01 | 2 | work | WIP: Hold the seam's trip trace down outside the hop check |
| `os-35/ipc-ring` | 2026-10-01 | 2 | work | WIP: Take the block ring's task off the data path (os-35 part B) |
| `os-35/ipc-lazytlb-on-ef206bb2` | 2026-10-01 | 1 | work | WIP: renumbered onto ef206bb2, generated docs not yet regenerated |
| `os-35/ipc-lazytlb-land` | 2026-10-01 | 1 | work | Keep the last program's space loaded under kernel threads where SMAP or PAN refuses user p |
| `backup/2026-10-01/os-35/ipc-lazytlb-land` | 2026-10-01 | 1 | snapshot | Keep the last program's space loaded under kernel threads where SMAP or PAN refuses user p |
| `os-35/ipc-lazytlb-wip` | 2026-09-30 | 1 | history | WIP lazy TLB for kernel threads |
| `os-35/ipc-lazytlb-rnc5` | 2026-09-30 | 2 | never land | NC5 scratch |
| `os-35/ipc-lazytlb-rnc4` | 2026-09-30 | 2 | never land | NC4 scratch |
| `os-35/ipc-lazytlb-rnc3` | 2026-09-30 | 2 | never land | NC3 scratch |
| `os-35/ipc-lazytlb-rnc2` | 2026-09-30 | 2 | never land | NC2 scratch |
| `os-35/ipc-lazytlb-rnc1` | 2026-09-30 | 2 | never land | NC1 scratch |
| `os-35/ipc-lazytlb-pre-measure` | 2026-09-30 | 1 | history | Keep the last program's space loaded under kernel threads |
| `os-35/ipc-lazytlb-on-eeaa` | 2026-09-30 | 1 | history | Keep the last program's space loaded under kernel threads |
| `os-35/ipc-lazytlb-nc3` | 2026-09-30 | 2 | never land | NC3 scratch |
| `os-35/ipc-lazytlb-nc2` | 2026-09-30 | 2 | never land | NC2 scratch |
| `os-35/ipc-lazytlb-nc1` | 2026-09-30 | 2 | never land | NC1 scratch |
| `os-35/ipc-lazytlb-571ea292` | 2026-09-30 | 1 | history | Keep the last program's space loaded under kernel threads |
| `os-35/ipc-lazytlb` | 2026-09-30 | 1 | history | Keep the last program's space loaded under kernel threads where SMAP or PAN refuses user p |

## Verification audit

`audit-2`: the second verification audit, WIP, with its mutants untriaged.
The ledger is `~/.local/share/ferrix/verification-audit/ledger.md` on nazuna;
read its last section first.

| Branch | Last commit | Unlanded | Kind | Tip |
|---|---|---:|---|---|
| `audit-2` | 2026-10-01 | 1 | work | WIP: audit 2 rows (N4 down to four controls; the queue's control mode keeps no diff) |

## Desktop, display, input, clipboard

- `display-design`: the ring-3 virtio-gpu display driver design, a draft for
  review. `display-show` (snapshots only) followed it.
- `clipboard-vdagent` and `wip/clipboard-vdagent-compositor-approach`: two
  implementations of the clipboard agent (`docs/CLIPBOARD.md` §8). Origin's
  `clipboard-vdagent` is the `user/vport` one; the other is a
  `compositor/vdagent` crate. Decide which to keep before resuming either.
- `edid-home-fix`, `wip/windows-home-edid-remote-fix`
  and `backup/2026-10-01/wip-root-main`: HOME on Windows, carrying a monitor's
  EDID across machines, and the remote-desktop tool. They are the uncommitted
  changes still in the Windows root checkout on 2026-10-01 (README, BACKLOG,
  roadmap charts, `xtask` `edid.rs`, `remote.rs`, `dotfiles.rs`,
  `compositor/run.rs`); compare before committing either.
- `hyprlock` and `hyprlock-full-4e813ac4`, `fuzzel-window-*`: the desktop
  clients (`docs/DESKTOP-CLIENTS.md`); hyprlock is on `main`, and these hold
  commits that are not: compare before resuming. `hyprlock`'s four WIP
  commits (P1.5) landed on 2026-10-03, replayed onto the relaid tree as
  `hyprlock-p15`, so that branch is history now.
- `guest-frame-time` and `guest-frame-time-local`: diverged. The first has
  six WIP commits on terminal and scroll rendering; the second is d33a86602,
  a guest frame-cost fix (rounding and `memcmp` without the C library).
  Both touch `compositor/render/exact.rs` and `compositor/term/*`; reconcile
  by hand, never force-push one over the other.
- `display-show` (snapshots only): its worktree's one uncommitted line turns
  `compositor/blank`'s background blue against its comment; it reads as a
  debug probe.
- `dk1-console/tx-fix` and `dk1-test`: the STM32MP157 board's console and
  input; the board needs a human at it.

| Branch | Last commit | Unlanded | Kind | Tip |
|---|---|---:|---|---|
| `wip/windows-home-edid-remote-fix` | 2026-10-01 | 1 | snapshot | Find HOME on Windows, carry a monitor's EDID across machines, and let a newer remote-deskt |
| `wip/clipboard-vdagent-compositor-approach` | 2026-10-01 | 1 | snapshot | Give the guest a clipboard agent, carried into the initramfs and started by hyprland |
| `edid-home-fix` | 2026-10-01 | 2 | work | Start the README's desktop with --everything, and count the points of 09-27 to 10-01 |
| `display-design` | 2026-10-01 | 1 | work | Draft the ring-3 virtio-gpu display driver design, for the product owner and kernel to rev |
| `backup/2026-10-01/wip-root-main` | 2026-10-01 | 1 | snapshot | Backup of uncommitted work in ROOT (main) before a PC switch, 2026-10-01 |
| `backup/2026-10-01/wip-display-show` | 2026-10-01 | 1 | snapshot | Backup of uncommitted work in display-show (detached) before a PC switch, 2026-10-01 |
| `backup/2026-10-01/wip-display-design` | 2026-10-01 | 1 | snapshot | Backup of uncommitted work in display-design (display-design) before a PC switch, 2026-10- |
| `backup/2026-10-01/wip-clipboard` | 2026-10-01 | 1 | snapshot | Backup of uncommitted work in clipboard (clipboard-vdagent) before a PC switch, 2026-10-01 |
| `readme-gui` | 2026-09-27 | 2 | work | WIP: uncommitted work found in the readme-gui worktree at the 2026-09-27 wind-down |
| `hyprlock` | 2026-09-27 | 4 | work | WIP hyprlock: rustfmt the auth backend and its tests |
| `frame-budget` | 2026-09-27 | 2 | work | File the lock boot that counted a bind after the unlock as one during it |
| `clipboard-vdagent` | 2026-09-27 | 1 | work | WIP: uncommitted work found in the clipboard-vdagent worktree at the 2026-09-27 wind-down |
| `hyprlock-full-4e813ac4` | 2026-09-26 | 7 | work | WIP hyprlock: lib.rs as slice 1 has it |
| `fuzzel-window-prerebase` | 2026-09-26 | 12 | history | fixup! Say in the desktop clients' design what fuzzel does with the user's file |
| `wip/display-show` | 2026-09-24 | 1 | snapshot | WIP snapshot of worktree display-show, 2026-09-24, before switching PCs |
| `wip/display-design` | 2026-09-24 | 1 | snapshot | WIP snapshot of worktree display-design, 2026-09-24, before switching PCs |
| `wip/clipboard` | 2026-09-24 | 1 | snapshot | WIP snapshot of worktree clipboard, 2026-09-24, before switching PCs |
| `dk1-console/tx-fix` | 2026-09-24 | 12 | work | Let a quiesce wait out a dead driver's end the kernel still holds |
| `dk1-test` | 2026-09-23 | 4 | work | hyprix: follow_mouse focuses the window under the pointer |
| `worktree-bridge-cse_017NVB1xDHbNpY7HnGonPpen` | 2026-09-20 | 7 | work | Let the runtime say which Linux table it is, not the library |
| `ivf-row` | 2026-09-19 | 1 | work | File the host's missing IVF wallpapers |
| `guest-frame-time-local` | 2026-09-18 | 1 | history | Round and compare without the C library, and paint a terminal's rows |
| `guest-frame-time` | 2026-09-18 | 6 | work | WIP scroll fix |
| `backup/2026-10-01/guest-frame-time` | 2026-09-18 | 1 | snapshot | Round and compare without the C library, and paint a terminal's rows |
| `worktree-bridge-cse_0134i6GzHzTtV9KMyY6YsBQh` | 2026-09-17 | 2 | work | Say in the handoff that the blur is done |
| `os-e5/input-l4` | 2026-09-17 | 1 | work | WIP: libs/virtio-input, docs/INPUT.md L4, written but never compiled |
| `os-e5/compositor-render` | 2026-09-17 | 1 | work | WIP: a tiny-skia renderer for the compositor, not yet built |
| `develop` | 2026-09-17 | 2 | work | Say in the handoff that the blur is done |

## ferrousli, ports, zinc

- `codex/posix-stdlib`, `ferrousli-netcore`, `ferrousli-netdb`: the POSIX
  gap work (quick_exit, secure_getenv, crypt, the resolver, name resolution).
  The `wip/` and `backup/` copies are their snapshots.
- `ferrousli-math`, `ferrousli-math-stubs`, `ferrousli-misc`,
  `ferrousli-patterns`, `ferrousli-threads`, `os-a8/long-double`: stopped
  unfinished at the 2026-09-13/-14/-17 wind-downs; much has landed since by
  other routes, so diff against `main` before resuming.
- `os-12/ports-autobuild` (also `ports-autobuild`): the ports built only
  when stale, Phase 3 A4 of `docs/TEST-TIME.md`. The other `os-12/*` branches
  are the curl, git and btop ports' earlier states; those ports are on `main`.
- `zinc`: the first zinc commit; zinc is on `main`.

| Branch | Last commit | Unlanded | Kind | Tip |
|---|---|---:|---|---|
| `ferrousli-netdb` | 2026-10-01 | 4 | work | Finish name resolution with ether_*, getifaddrs/if_nameindex, and addrinfo/hostent |
| `ferrousli-netcore` | 2026-10-01 | 3 | work | Rework the resolver's DNS, lookup and inet pieces, and drop what stubs.rs covered |
| `codex/posix-stdlib` | 2026-10-01 | 1 | work | Add quick_exit, secure_getenv, a64l/l64a, getsubopt, and crypt's DES ciphers |
| `backup/2026-10-01/wip-ferrousli-netdb` | 2026-10-01 | 1 | snapshot | Backup of uncommitted work in ferrousli-netdb (ferrousli-netdb) before a PC switch, 2026-1 |
| `backup/2026-10-01/wip-ferrousli-netcore` | 2026-10-01 | 3 | snapshot | Backup of uncommitted work in ferrousli-netcore (ferrousli-netcore) before a PC switch, 20 |
| `backup/2026-10-01/wip-codex-posix-assert` | 2026-10-01 | 1 | snapshot | Backup of uncommitted work in codex-posix-assert (codex/posix-stdlib) before a PC switch,  |
| `wip/ferrousli-netdb` | 2026-09-24 | 1 | snapshot | WIP snapshot of worktree ferrousli-netdb, 2026-09-24, before switching PCs |
| `wip/ferrousli-netcore` | 2026-09-24 | 3 | snapshot | WIP snapshot of worktree ferrousli-netcore, 2026-09-24, before switching PCs |
| `wip/codex-posix-assert` | 2026-09-24 | 1 | snapshot | WIP snapshot of worktree codex-posix-assert, 2026-09-24, before switching PCs |
| `ports-autobuild` | 2026-09-17 | 3 | work | wip: build the ports with a pinned clang where gcc is older than 15 |
| `os-a8/long-double` | 2026-09-17 | 1 | work | WIP: the x87 long double foundation, unbuilt |
| `os-12/ports-autobuild` | 2026-09-17 | 3 | work | wip: build the ports with a pinned clang where gcc is older than 15 |
| `os-50/dev` | 2026-09-16 | 1 | never land | dev build |
| `os-12/probe` | 2026-09-16 | 3 | never land | probe: https in the guest (not for landing) |
| `os-12/ports-curl-btop` | 2026-09-16 | 1 | history | Port git onto ferrousli, and clone over HTTP inside the guest |
| `os-12/ports` | 2026-09-16 | 1 | history | Port git onto ferrousli, and clone over HTTP inside the guest |
| `os-12/autobuild` | 2026-09-16 | 1 | work | wip: build the ports when stale |
| `zinc` | 2026-09-14 | 1 | history | Start zinc, a zsh-compatible shell in Rust (WIP) |
| `os-50/ferrousli-patterns` | 2026-09-14 | 2 | work | WIP: dirname from ferrousli-misc and regex, stopped unfinished at the 2026-09-14 wind-down |
| `os-50/ferrousli-math-stubs` | 2026-09-14 | 1 | work | WIP: sin, cos, exp, log, pow, atan2 and the __fpclassify/__signbit helpers, stopped unfini |
| `ferrousli-patterns` | 2026-09-14 | 2 | work | WIP: dirname from ferrousli-misc and regex, stopped unfinished at the 2026-09-14 wind-down |
| `ferrousli-math-stubs` | 2026-09-14 | 1 | work | WIP: sin, cos, exp, log, pow, atan2 and the __fpclassify/__signbit helpers, stopped unfini |
| `ferrousli-threads` | 2026-09-13 | 1 | work | Keep the unbuilt barriers, semaphores, sched.h and C11 threads |
| `ferrousli-misc` | 2026-09-13 | 2 | work | Keep the unfinished search.h, libgen, fnmatch, glob and regex work |
| `ferrousli-math` | 2026-09-13 | 2 | work | Keep the unfinished math library and fenv as builds are halted |
| `enosys-sweep` | 2026-09-13 | 2 | work | Let the sweep's shell print more unanswered lines before init stops it |

## Steam, Chrome, GPU, the phone

- `wip/steam-*` and `ferrousli-steam`: Steam's store and game steps
  (`docs/roadmap/stage-22-steam.md`); `test-steam-game` landed as 99bfb7222,
  not passing yet.
- `os-ac/vulkan` and `os-ac/venus-wip`: vkgears through Venus (G3-G5 left).
- `chrome-groundwork`: the Chrome assessment's re-check.
- `pixel7-vm-try`, `pixel7-chromium`: the Pixel 7's VM desktop and Chromium's
  AArch64 signal return.

| Branch | Last commit | Unlanded | Kind | Tip |
|---|---|---:|---|---|
| `ferrousli-steam` | 2026-09-30 | 1 | work | File test-xwindow failing on main under Red on main |
| `backup/2026-10-01/ferrousli-steam` | 2026-09-30 | 1 | snapshot | File test-xwindow failing on main under Red on main |
| `wip/steam-y6` | 2026-09-29 | 1 | snapshot | WIP snapshot: WM_NORMAL_HINTS, a fixed-size window floats |
| `wip/steam-userns` | 2026-09-29 | 2 | snapshot | WIP snapshot: N2 bind mounts, host tests in progress |
| `wip/steam-e2e` | 2026-09-29 | 2 | snapshot | WIP snapshot: Steam window harness into the repo |
| `os-ac/vulkan` | 2026-09-24 | 1 | work | WIP: vkgears port |
| `os-ac/venus-wip` | 2026-09-24 | 14 | history | WIP: dispatch patch |

## Kernel, init, storage, boards, certification work

- `init/l4`: libs/svc's manager (cgroups and init, L4 next).
- `btrfs-write`: stage 12's power-fail work.
- `stage8-filemmap`, `vmo-map-wip`, `stage10-ring`,
  `stage11-kernel-mount-design`: mm and storage work stopped on 2026-09-13/-14;
  check `main` first, much of it was later done differently.
- `os-8d/selfhost`: stage 20, self-hosting.
- `boot-21b` (W-8 boot, full gate green on cf13918f; left: the review, a
  rebase, the carry) and `w8-armv7a`: certification requirement work.
- `f46-power-off`: F-46, parked with its controls run; message, rebase, gate
  and the consultant left.
- `unix-creds` (also `os-02/unix-creds`): `SCM_CREDENTIALS`, never compiled.
- `worktree-agent-a33c10946b6721065`: the panic QR code.

| Branch | Last commit | Unlanded | Kind | Tip |
|---|---|---:|---|---|
| `init/l4` | 2026-10-01 | 1 | work | Give libs/svc a manager: dependency edges, lifecycle, and a restart policy |
| `backup/2026-10-01/wip-init-l4` | 2026-10-01 | 1 | snapshot | Backup of uncommitted work in init-l4 (init/l4) before a PC switch, 2026-10-01 |
| `w8-armv7a` | 2026-09-27 | 1 | work | wip: W-8 file 24 drafts (arch/armv7a, arch/arm_common), parked for the wind-down |
| `f46-power-off` | 2026-09-27 | 2 | work | WIP F-46: controls run, qemu.rs .get() fix, evidence edits; not reviewed, not rebased |
| `boot-21b` | 2026-09-27 | 1 | work | Write the boot's low-level requirements, and count ferrix.checks as product (W-8 boot, 21b |
| `wip/init-l4` | 2026-09-24 | 1 | snapshot | WIP snapshot of worktree init-l4, 2026-09-24, before switching PCs |
| `sysml-studio-submodule` | 2026-09-23 | 4 | work | Move SysML Studio to its settings and shortcuts |
| `os-8d/stage20-before-rebase` | 2026-09-23 | 11 | history | wip: plan test covers reads |
| `os-8d/selfhost` | 2026-09-23 | 12 | work | wip: boot check that mmap waits for the layout |
| `btrfs-write` | 2026-09-21 | 8 | work | WIP stage 12 powerfail |
| `unix-creds` | 2026-09-17 | 2 | history | WIP 3c SCM_CREDENTIALS/SO_PASSCRED, written but never compiled |
| `os-26/fx1151-diag` | 2026-09-17 | 1 | never land | DIAGNOSTIC: net ring exit reasons (not for main) |
| `os-02/unix-creds` | 2026-09-17 | 2 | work | WIP 3c SCM_CREDENTIALS/SO_PASSCRED, written but never compiled |
| `os-02/fx0701-diag` | 2026-09-16 | 1 | never land | WIP FX-0701 diagnostics |
| `fx0701` | 2026-09-16 | 1 | never land | WIP FX-0701 diagnostics |
| `stage9/log-header` | 2026-09-14 | 1 | work | WIP: Name the tree a gate ran on as the first line of its log |
| `stage8-filemmap` | 2026-09-14 | 4 | work | WIP: file mmap, what is left of MAP_SHARED and MAP_PRIVATE (8 + 8 points) |
| `worktree-agent-aa3e651b1a5027598` | 2026-09-13 | 3 | never land | WIP: temporary hang instrumentation for single-CPU Arm, not for landing |
| `worktree-agent-a33c10946b6721065` | 2026-09-13 | 1 | work | WIP: port Linux's drm_panic_qr as ferrix-qr, for drawing a panic report |
| `worktree-agent-a293f44fe214785df` | 2026-09-13 | 4 | work | WIP: check the leak counters the fuzz-hardened handlers only printed |
| `vmo-map-wip` | 2026-09-13 | 2 | history | WIP: keep vmo_map's check and remap under clippy's length limit |
| `stage8-diag` | 2026-09-13 | 2 | never land | DIAGNOSTIC, not for landing: count frames by release route across two self-check windows |
| `stage11-kernel-mount-design` | 2026-09-13 | 1 | work | WIP: keep stage 11's kernel-mount design where a restart cannot lose it |
| `stage10-ring` | 2026-09-13 | 2 | work | WIP: the block ring's kernel side, unbuilt |
| `sched-diag` | 2026-09-13 | 1 | never land | Charge the running task before a queue insert, honour a reschedule owed from an empty reap |
| `os-c4/diag` | 2026-09-13 | 2 | never land | DIAGNOSTIC, not for landing: count frames by release route across two self-check windows |

## os-5d: superseded by main

os-5d's work landed as 7afed6fdc on 2026-10-01 (`--iterate`, `gate-rows`, the
compositor's first-boot checks, `docs/TEST-TIME.md`'s Windows host). These are
its older copies; delete them.

| Branch | Last commit | Unlanded | Kind | Tip |
|---|---|---:|---|---|
| `os5d/gate-rows` | 2026-10-01 | 1 | superseded | Record the Windows host's first cold gate runs, and why neither finished |
| `backup/2026-10-01/wip-os5d-gate-rows` | 2026-10-01 | 1 | superseded | Backup of uncommitted work in os5d-gate-rows (os5d/gate-rows) before a PC switch, 2026-10- |

## Already on main

Every change on these branches was on `main` under another hash. All of them
were deleted from origin on 2026-10-05 (tips in
`~/.local/share/ferrix/branch-archive-2026-10-05/origin-all.bundle`; see
*2026-10-05, evening: refreshed*): `backup/2026-10-01/os5d/gate-rows`,
`backup/2026-10-01/os5d/profile-iterate`, `backup/2026-10-01/po-winddown`,
`os5d/compositor-skip`, `stage13-n4-userns`, `stage13-s1`, `stage13-todos`,
`wip/steam-procfs-fd`, `pixel7/bootloader`, `os-d1/monitor-transform`, `omz`,
`omz-local-2026-09-27`, `wallpaper-settings`, `os-a4/compositor-damage`,
`os-02/smmu`, `os-50/batch3b`, `os-50/batch3c`, `os-50/ci`, `os-50/land`,
`os-a8/kept-os02-stub-462f7714`, `os-50/wscanf`, `os-12/btop5`,
`os-50/ferrousli-link-breakers`, `os-50/ferrousli-netdb`,
`pre-pull-backup-2026-09-13`, `foot-shell`, `selfhost-components`,
`tonight/foot-shell`, `tonight/l13-init`, `tonight/selfhost-matrix` and
`tonight/stage13-s3-on-netns`.

## Not on a branch

- **The test-badapple armv7a window boot is red**, not owned: red since at
  least 594d69146; "holding frame 359" while the dump shows an earlier frame
  (62057 of 196608 pixels wrong), hyprix drawing in software on armv7a. The
  window path was only ever gated on x86_64 (ferrix-db, 2026-10-04).

- **The gate pool's deadlock fix** for `~/.local/share/ferrix/fleet/gate.sh`
  on nazuna, which is not in the repository: a run held to one slot
  (`GATE_SLOT`) must not hold the queue (`docs/TEST-TIME.md`, *Deadlock*).
  Not installed.
- **The Windows host as a gate** (`docs/TEST-TIME.md`, *The Windows host as a
  gate*): a cap on WSL's target dirs or the WSL disk off C:, a C: disk guard,
  then a cold and a warm `check` measured to the end.
