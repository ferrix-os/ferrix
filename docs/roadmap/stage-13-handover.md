# Stage 13 handover (2026-10-01)

Stage 13 (namespaces, cgroups v2, seccomp) stopped at the customer's wind-down
on the evening of 2026-10-01. This page is for the session that takes it over:
what is on `main`, where every unfinished branch is, what each one still owes,
and how to land it. The stage's roadmap entry is
[stage-13-namespaces-cgroups-v2-seccomp.md](stage-13-namespaces-cgroups-v2-seccomp.md);
the designs are `docs/NAMESPACES.md`, `docs/PIDNS.md`, `docs/NETNS.md`,
`docs/CGROUPS.md` and `docs/SECCOMP.md`.

The exit criterion -- an unprivileged user namespace runs a pid 1 under a
memory limit with a scoped OOM kill and a seccomp filter that blocks a call --
is **not met**. It needs cgctl and S3 on `main`, then `stage13-container` run.

## On main

| Landing | Commits |
|---|---|
| N4, user namespaces | d171ffe5 |
| S1, the classic-BPF checker and interpreter (`src/lib/kernel/seccomp`) | 63b0e70c, 5fd9c1fb, and the review's fixes in 97944eb7 |
| S2, the filter hook at all four system-call entries | 6f47bbd2..ff5e45ef |
| The small namespaces (UTS, IPC, cgroup), nsfs and `setns` | 63f9f97e5, 8cfa36b00 |
| Pid namespaces | a2af5061a, d3ffdc467 |
| `L.trap.8` reserved for S4 | d229861b0 |
| Network namespaces (2026-10-04) | 8afc45211..81f68f8ff, landed in 22384874f |

## Unfinished branches, where to take over

Every branch is on `origin` with the tip below, committed and clean. Each also
has a local worktree under `.claude/worktrees/<dir>` on the Windows checkout;
make your own if you work elsewhere. They are listed in landing order within
each chain; a later branch in a chain sits on the earlier one.

### cgroup controllers

**2026-10-05:** po6-cgctl carried this on as `po6/cgctl` (69fe7b51c on
`main`'s 3349682db) and `po6/cgctl-n6` (c4e2b8f9d on `land-n6`), both on
origin: 23 controls FIRED, the gate rows passed, finding D1 fixed. Owed: the
consultant's look at the D1 fix and the four new controls, then the batch. The
row below is the 2026-10-01 state it started from.

| Branch | Tip | Worktree | State | To land it |
|---|---|---|---|---|
| `stage13-cgctl` | 670b4c49a | `cgctl` | M2's reclaim and `memory.high`, `cgroup.freeze`, `cpu.max`, the `io` controller. Every gate row PASSED on 89c2f911a (INDEX `cs-*`, x86_64 under kvm and the release build included). Controls FIRED: io-charge, io-parent, io-throttle, io-root, io-limit, io-ended, cpu-throttle, reclaim-hole. | Run the 11 controls not run (cpu-kill, cpu-charge, cpu-rearm-write, cpu-rearm-move, freeze-park, freeze-sigcont, park-poll, reclaim-none, reclaim-sibling, reclaim-min, dentry-keep on armv7a); reclaim-sibling has never shown its own message. Update CGROUPS.md §14's control table. Have the consultant see the `cpu.max` bound widened from two fifths to two thirds of a processor. Delete its entry from `tools/common/data/requirement-reservations.json` in the commit carrying L.object.106-112, L.sched.3-4, H.QUOTA.10-12. Nazuna worktree `os-cg-wt` and side refs `os-cgctl/*` are kept for this. |

### seccomp (S3 lands first)

**2026-10-04: resume S3 from `stage13-s3-on-netns` 0527dd365**, not from the
rows below. It is S3 on main 22384874f with two fixes: the table sizes (255,
223, 273) and the filters check listing its tasks. Its rows passed on
6c539b276 (`l13s3f-*`), all 19 controls FIRED, consultant OK at ledger line
336; k7, k13 and k17 FIRED on 0527dd365 (`l13s3n2-*`) and the rest carry
(`~/.local/share/ferrix/logs/s3-range-diff.txt`). Owed: one batch re-run
(`fleet/batch.sh join`, gate file `~/.local/share/ferrix/logs/l13-s3-gate.txt`)
and the consultant's final OK. Handover: `~/.local/share/ferrix/l13-coord/HANDOVER.md`.
The two rows below are history.

| Branch | Tip | Worktree | State | To land it |
|---|---|---|---|---|
| `stage13-s3` | d19606300 | `s3` | Filters, strict mode, the actions. check and three boots PASSED on 6ec5b4081; controls k1-k5, k11-k15 FIRED. Consultant: cleared on evidence. | The consultant refused a reduced gate: six rows (check, three boots, `test-threads`, `test-init`) must PASS on the rebased hash 8ffcc278a, plus all 19 `s3k` controls FIRED. |
| `stage13-s3-onmain` | 8ce395f53 | `s3m` | The same patches rebased onto main 21f580cad (range-diff all `=`). | Land this one once its rows pass. |
| `stage13-s4` | 9ad2c718e | `s4` | `SECCOMP_RET_TRAP`. check PASSED, t1 FIRED on 5e0ba9792. Consultant: OK if five conditions; 1, 2 and 5 written. | Conditions 3 and 4, controls t2-t7, the rows after S3. Delete the `L.trap.8` reservation in the commit that writes it. |
| `stage13-s5` | 298399f1e | `s5` | `TSYNC`. Consultant: OK if; a new thread starting with its creator's chain is written. | A measured bound for the TSYNC ancestor walk (it runs with preemption off), the rows, four controls. |
| `stage13-s6` | 64e891f7e | `s6` | `cargo xtask test-seccomp` (S6a), passed on all four ABIs in a direct run. Needs no review. | gate.sh INDEX lines. Linux's `seccomp_bpf` selftest is a BACKLOG row. |

### /proc access and unprivileged mounting (fdinfo first)

| Branch | Tip | Worktree | State | To land it |
|---|---|---|---|---|
| `stage13-fdinfo` | 225eb2410 | `fdinfo` | `/proc` by `ptrace_may_access`, dumpable, `/proc/<pid>/fdinfo` (NP squashed in). Rebased on fcc7af7de. check, three boots (armv7a at two and four processors), `test-init`, `test-shell` PASSED (`fdi23-*`); k1, k2 (fdi23) and k3, k6, k7 (fdi18) FIRED. Consultant: OK if four conditions. | `test-vfs` on fdi23's hash; k4 and k5 with `--expect` on the message that fires first; k8-k17 FIRED on the landing hash; NAMESPACES §12 quoting the controls' INDEX names. |
| `stage13-n5` | 7a0c04fdd | `n5` | Unprivileged mounting, on fdinfo. The consultant asked for changes; built: a plain remount needs privilege over the superblock's owning namespace, the bottom mount's flags are locked, a final write-out checks it may sleep. Earlier tip's boots, `test-vfs`, `test-shell`, `test-bwrap` PASSED; m2, m2f, m3 FIRED. | The consultant's final word is **not yet**: it judged the plain-remount rule still to be "every mount of this filesystem is in my namespace" rather than privilege over the superblock's owning user namespace, as the 2026-09-30 review required; also open are a write-out that may sleep in `Namespace::drop`, the bottom mount's unlocked flags, and the controls. Check which of these the tip already fixes, then gate it and ask again. |
| `stage13-bwrap-user` | dad30e7f2 | `bwrapu` | `test-bwrap` as uid 1000; passes since a new tmpfs is 1777 and owned by its mounter. | Rebase onto `stage13-n5` (conflicts in `fsctl.rs`, `mountperm_check.rs`), a control for the tmpfs case, review. |

`stage13-np`, `stage13-fdinfo-v2`, `-v3` and `-presquash` are superseded by
`stage13-fdinfo`; keep them only as history.

### network and time namespaces (netns first)

| Branch | Tip | Worktree | State | To land it |
|---|---|---|---|---|
| `stage13-netns` | landed | `netns` | Landed 2026-10-04 on def906ba2, gated on 2428b0d95 (`l13ns-*`). | -- |
| `stage13-timens` | 88b073c3a | `timens` | Time namespaces, on netns. Boots on x86_64. A native child gets its creator's time namespace and shifted vDSO (controls c25, c26). BACKLOG rows written. Not reviewed. | Its `ae24` check and `test-init` rows FAILED; find why. The consultant's open question: the vDSO swap answers Ok when the shifted vDSO is missing, which may fail open. Then the 26 controls (`ae-tn-*`; c01, c03-c07 FIRED so far; c02 and c08 need `--expect` on the native-child message) and review. |

### exit criterion

| Branch | Tip | Worktree | State | To land it |
|---|---|---|---|---|
| `stage13-container` | cda83faef | `container` | `cargo xtask test-container`, the exit criterion as one program. Written, never built or run. | Needs cgctl and S3 on `main`; then build, run on three architectures, land. |

### Not started

N6 and N7: Steam as uid 1000, and pressure-vessel.

### History only, do not land

`stage13-seccomp` (the first in-dispatcher attempt, rejected as a sandbox that
lies), `stage13-s3-wip`, `stage13-integ`, `stage13-landing`,
`stage13-n4-userns`, `stage13-s1`, `stage13-s1b`, `stage13-todos`: their
content is on `main` or replaced by the branches above.

## How to land one

1. Rebase onto `origin/main`; `main` moves often. Keep every other branch's
   lines in `launch::load_native`: every namespace and seccomp review found that
   a native child must get its creator's whole namespace set, pid namespace,
   seccomp chain and `no_new_privs`.
2. Gate through `~/.local/share/ferrix/fleet/gate.sh` on nazuna (`run`,
   `control --expect "<panic text>"`, `status`) after pushing a side ref. A row
   counts when its INDEX line says `run: PASSED` on the commit that lands, with
   the accelerator named; a control counts when it says
   `control: FIRED (panic): <line>` or `FIRED (line, no panic)`. Verdicts from
   before about 18:40 that day were unreliable. Each gate slot needs
   `build-apps --app sshdt` before `test-init` passes there. A job pinned with
   `GATE_SLOT` to a slot shed for low disk blocks the whole queue (a known
   gate.sh bug).
3. Send the certification consultant (`AGENTS.md` names who holds the role)
   the branch, commit and INDEX lines before `land.sh take`. Its ledger of
   every verdict and condition (the wind-down consultant's final word on each
   branch is in `HANDOVER.md` there; the previous one is
   `HANDOVER-2026-10-01-osbd.md`) is
   `~/.local/share/ferrix/cert-consultant/reviews.md` on nazuna, with
   `HANDOVER.md` beside it. Requirement ids are reserved in
   `tools/common/data/requirement-reservations.json` before they are written.
4. Take the lock (`~/.local/share/ferrix/fleet/land.sh`, never piped), assert
   `git merge-base --is-ancestor origin/main HEAD`, push `HEAD:refs/heads/main`,
   release, and update the roadmap entry.

## Size of what is left

About 40 to 45 points without N6 and N7 (one point is 20 to 30 minutes of one
session), about 5 to 7 hours with four landing chains and a consultant working
at once. Most branches have needed one review round; a second adds about an
hour to that branch.
