# B5's check host (ledger 5782 (11))

**For the consultant, 2026-11-04 (mk-m2).** Class B's ids (`B5.md` §4) are about what a store or a trap does
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
* **Who calls it**: stage 9 after K11's line, a function `stages_check::check_bare`, on every profile, with a
  `bare` boot line, no Linux-ABI `cfg`, so `present` and `fast` run the same check.
* **Class B's ids on it**, each with a sabotage in product code: L.sched.30, 31, 33, 35, 80, 81, 82, 3, 4; L.trap.2;
  L.user.61, 87, 96; L.x86_64.55, 59, 68, 79; L.object.100, 107, 108; H.QUOTA.6, 10, 11, 12; H.SCHED.14; H.MEM.8; and the
  kernel halves of L.syscall.20-23 (5782 (4)). A statement that names Linux's `fork` or a signal keeps its load
  check for the hybrid and takes its kernel half by `SPACE_CLONE` or a kick, under review, not SP-3.

Questions: Q1, is a second harness acceptable? Recommendation: yes, for tonight's rebases; `Side` moves onto it at
M5. Q2, is `Bare` in `present` deactivated code without this? K11's own check runs it, so no. Q3, the 299 stay on
the ratchet with this as their way down.
