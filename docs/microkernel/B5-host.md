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
  line; M5's gate does not read it as met before then. `run` answers 2 on ARMv7-A (`over_its_mark_a_job_gives_back` and
  `a_job_says_who_is_frozen`, the two with no program, since it has no K11 and its programs do not run; the program
  is kept for M5), 11 or 13 on AArch64 and x86-64 (the two time cases where the boot counts kernel time), and the
  arguments case on x86-64 alone. So L.object.107 and 108 count on all three; L.x86_64.55, 68 on x86-64; the rest on
  AArch64 and x86-64. `attempted`'s time cases read a fraction of time: one run in three holding passes, so a charge
  wrong two times in three would (6946 (2)). Dispatch's calls claim nothing about the entry; the
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
| L.sched.3, 4 | a job's `cpu.max` set by the check, a spinner its end stops; no Linux signal in the wait's cases | restated: "a signal" to its kernel half |
| H.QUOTA.10, L.object.106 | the same run's periods and `cpu_times`; the tick's user-or-system half is L.sched.80's | restated (6142 (8)) |
| L.sched.35 | a spinner on another processor and one here, ended by `Bare::end` | restated: "a program past its memory.max" to any end |
| L.sched.30, 31, 33 | a kick and END on a `Bare`; STOP and the audit stay with the personality's `needs_attention` | not yet tagged: ask |
| L.object.108 | a file over a check `Filler`, pages taken as the job, `oom::throttle` called as the trap's way out does | SP-3 |
| L.user.62, H.MEM.16 | a clone's own stores to two shared pages and a private one | restated: "fork" to the clone |
| H.TRAP.16, L.aarch64.51, L.armv7a.1, L.x86_64.124 | the entries' drive is raised and the core's slot holds seccomp's rule, which a kernel with no load does not register | no route yet: ask |
| L.object.100, 108, H.QUOTA.6, 12 | a sourced `Vmo` over a check `PageSource`, cache charged to the job, as a thread of the `Bare` | restated where they name a Linux status |
| L.user.61, H.MEM.8 | two `Bare`s on S and its clone, each a program's own store; replaces stage 6's `USER_COW_PROGRAM` in user builds (L.user.160, 6110 (7)) | restated: "fork" to the clone |
| L.user.96 | K2 cannot make a private `Backing::File`: class C, successor `linuxd`'s private-file row | moved, here first |
| L.object.107 | `Process::sync_freeze` on `Bare`s born, moved and thawed; `Bare` does not honour freeze (`needs_attention` is `own_end`), so the parking and SIGCONT stay the load check's, and H.QUOTA.11 is not tagged here | restated, its core half |
| L.x86_64.55, 68 | a native call's four registers; the start argument: K15's L.object.293 program on `Bare`; the control on the four changes R10 in the entry's Rust, not the assembly's own lines | SP-3 |

Controls that fire elsewhere than the case's sentence: `v-59b`, no preemption of a program with no region: the first case to notice is `a_quota_cuts_a_program_alone`'s "was not held to its cpu.max", and the two spinners' sentence is not shown to fire; `v-35a`, an end not posted: "a program did not end within the check's patience", the earlier case's (`v-35b`, narrowed, gives `an_end_reaches_every_thread`'s own). `t-61`, a private store made shared: the guard "a VMO backing a private region would have more than one mapper" fires in the clone, and "reached another holder" is not shown to fire. A real failure of a case with a spinning program can take `PATIENCE_NANOS`, 120 s, as long as `test-boot`'s own wait: BACKLOG's row.
