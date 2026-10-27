# Stage 14 — Real-time domains  ·  *month*

The `SoftRt` and `HardRt` modes: FIFO/RR classes, threaded interrupts,
priority-inheritance mutexes, EDF with constant-bandwidth admission control,
and the runtime domain-mode switch with its quiescence protocol.

**Exit:** a cyclictest-shaped boot test measuring wake-up latency on a `HardRt`
domain while a `Throughput` domain on other cores is saturated, reporting a
maximum inside the stated bound; plus an admission test that refuses an
unschedulable set instead of missing deadlines.

**Research (2026-10-26):** [docs/REALTIME.md](../REALTIME.md) surveys what
Linux, seL4 MCS, QNX, Zircon and the ARINC 653 systems do, what Ferrix has
today, and proposes the stage's design and an order of landings.

**Built so far (2026-10-27, on `claude/ferrix-roadmap-completion-hkxlpp`,
landing owed):** the groundwork every real-time class sits on,
[docs/SCHED-CLASSES.md](../SCHED-CLASSES.md), each step reviewed by the
certification consultant:

* `ferrix-sched-class`, the compile-time `SchedClass` trait every
  scheduler implements, and the class stack (`Over`, `Empty`);
* `ferrix-sched-fair`, EEVDF moved behind that trait, with the core's
  queue, run slot and hand-over going through it, at no cost to the IPC
  fast path (its size unchanged in the release ELF);
* the per-tier build flags (`sched_dispatch`, `sched_windows`,
  `sched_deadline`, `sched_realtime`, `sched_fair`) replacing `schedule`,
  the selection crate that composes the stack from them, the profiles'
  `modes`, and the boot line naming the stack;
* the evidence that a class crate a build leaves out is not linked
  (test-removable's class-crate half and a fixture crate);
* a stack of two tiers (step 4a), and on it `ferrix-sched-fifo`,
  `SCHED_FIFO` and `SCHED_RR` above fair on `desktop` (step 4b, in
  review): priorities 1 to 99, preemption on a wake, RR's 100 ms quantum,
  the 950-of-1000 ms floor that keeps fair tasks 50 ms a second, Linux's
  policy calls with `CAP_SYS_NICE` and `RLIMIT_RTPRIO`, reset-on-fork, and
  boot checks plus `cargo xtask test-rtsched` for each.

`max` has no fair tier, the customer's decision (2026-10-27). Not built
yet: a `SoftRt` domain of its own (compiled in, refused by name), the
EDF/CBS and windows classes, threaded interrupts, priority inheritance,
the mode switch and the exit tests above.

---

