# B5's check host (ledger 5782 (11))

**OK IF (1)-(8), ledger 6110, 2026-11-04 (mk-m2). Nothing lands from this side branch until K11 is on `develop`.** Class B's ids (`B5.md` §4) are about what a store or a trap does
from user mode, and today's harness (`object/check.rs`'s `Side`, `syscall/check.rs`'s `spawn_in`) makes a Linux
`Process`, so it is raised and does not link in `personality=user`.

* **No new `Host`.** K1 and K11's `object::bare::Bare` (reviewed at 5806) is the core's process that holds no
  personality; it is what a native process is under `personality=user`. One type.
* **A second, small harness, not a rewrite of `Side`.** `syscall/bare_side_check.rs`, item ring (it reaches
  `native::dispatch`, which is the item's; item, not raised), holding `BareSide`: a `Bare` in a job of the
  check's, `call` through the dispatcher, `put`/`get` on its space, a program mapped by `first_image` or K2, run
  by `process_run`'s path or `Bare::start`, `end`. `Side` is left alone: rewriting it moves 30 check files and
  their coverage, on the night five agents rebase over them. So `raised_only`'s 299 fall only as checks are
  written or moved onto `BareSide`, each by SP-3; the ratchet holds meanwhile, and the gate stays M5's.
* **Programs**: small stubs per architecture in that file, exiting by `THREAD_EXIT` (K11): spin, make calls in a
  loop, fault in a loop, store twice. No Linux number, so a run proves the kernel half alone.
* **Who calls it**: stage 9 after `build`, `stages_check::check_bare`, `FX-0936`, a `bare` boot line, on `present`
  and `desktop` today. **It does not run in `fast` yet** (6110 (1)): on `feature/mk-lift` `stages_check` builds only
  with the load in ring 0, and a user boot prints `UNCHECKED_MARKER`. So an id re-homed here counts for `present`
  and `desktop` until a caller in item or core is agreed with mk-lift and a `--personality user` row prints the
  line; M5's gate does not read it as met before then. ARMv7-A has no K11, so its programs do not run: `run`
  answers 0 cases there (the program for it is kept for M5). Dispatch's calls claim nothing about the entry; the
  stubs are that evidence, and a charge or job case runs as a thread of the `Bare` (6110 (5)).
* **Class B's ids on it**, each with a sabotage in product code: L.sched.30, 31, 33, 35, 80, 81, 82, 3, 4; L.trap.2;
  L.user.61, 87, 96; L.x86_64.55, 59, 68, 79; L.object.100, 107, 108; H.QUOTA.6, 10, 11, 12; H.SCHED.14; H.MEM.8; and the
  kernel halves of L.syscall.20-23 (5782 (4)). A statement that names Linux's `fork` or a signal keeps its load
  check for the hybrid and takes its kernel half by `SPACE_CLONE` or a kick, under review, not SP-3.

Questions: Q1, is a second harness acceptable? Recommendation: yes, for tonight's rebases; `Side` moves onto it at
M5. Q2, is `Bare` in `present` deactivated code without this? K11's own check runs it, so no. Q3, the 299 stay on
the ratchet with this as their way down.

## Routes and review, id by id (6110 (3), (4))

| Id | Route | SP-3 or restated |
|---|---|---|
| L.sched.80, 81, 82 | the program's spins, calls and faults; `Task` read after it ends | SP-3 |
| L.trap.2 | pages touched, the program goes on to its status | SP-3 |
| L.x86_64.59, 79 | two spinners on one processor, each cut | SP-3 |
| L.user.87 | `map_anonymous` on `own_space`, protected read-only by the check, the store faults | SP-3 |
| L.sched.3, 4 | a job's `cpu.max` set by the check; no Linux signal in the wait's cases | restated: "a signal" to its kernel half |
| L.sched.30, 31, 33, 35 | a kick and END on a `Bare` | restated: they name the personality's `needs_attention` |
| L.object.100, 108, H.QUOTA.6, 12 | a sourced `Vmo` over a check `PageSource`, cache charged to the job, as a thread of the `Bare` | restated where they name a Linux status |
| L.user.61, H.MEM.8 | two `Bare`s on S and its clone, each a program's own store; replaces stage 6's `USER_COW_PROGRAM` in user builds (L.user.160, 6110 (7)) | restated: "fork" to the clone |
| L.user.96 | K2 cannot make a private `Backing::File`: class C, successor `linuxd`'s private-file row | moved, here first |
| H.QUOTA.11, L.object.107 | `Bare` does not honour freeze (`needs_attention` is `own_end`): split with mk-restricted; the parking scoped, its successor named | restated |
| L.x86_64.55, 68 | a native call's four registers; the start argument: K15's L.object.293 program on `Bare` | SP-3 |
