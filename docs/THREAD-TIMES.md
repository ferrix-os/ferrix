# Thread times: kernel time, switch counts, and a sampling profiler

The first steps of a per-thread profile on Ferrix
(`docs/backlog/p2.md`, *A per-thread profile of a program cannot be taken
on Ferrix*). The customer's goal behind it: Endfield playable on the N150
box, where the GRYPHLINK launcher's video decode took 4.9 times a Zen 5
core's time against about 2.7 expected, and Ferrix could not say where
the difference went.

Ids: `H.SCHED.14` and `L.sched.80-83`, reserved on `main` at d04cff26a and ff18add48.

## 1. What is missing today

* `/proc/<pid>/stat` and `/proc/<pid>/task/<tid>/stat` report all of a
  task's time as `utime`; `stime` is always 0. The scheduler keeps one
  counter a task (`sched/task.rs`, `sum_exec`), charged at every decision
  (`CpuQueue::account_with`). Only a job's `cpu.stat` is split, by asking
  at the tick whether the processor was in user mode.
* `status` has no `voluntary_ctxt_switches` or
  `nonvoluntary_ctxt_switches`, so a thread that blocks cannot be told
  from one that is preempted. `Task::preemptions` counts only switches
  forced by an interrupt in user mode, and nothing reads it but a check.
* No user instruction pointer can be sampled: no `ptrace` sampling, no
  `perf_event_open`, `kstkeip` 0.

## 2. Kernel time, counted at the boundary

**What Linux does.** By default (`CONFIG_TICK_CPU_ACCOUNTING`) Linux
samples the mode at each tick and scales the split to the exact runtime
(`cputime_adjust`). With `CONFIG_VIRT_CPU_ACCOUNTING_GEN` (nohz_full
processors) it reads the clock at every kernel entry and exit instead.
Ferrix has no periodic tick -- a processor running one task with nothing
behind it gets none (`sched::charge_running`'s comment) -- so a sampled
split would read nothing for exactly the threads to be profiled. Ferrix
does what `VIRT_CPU_ACCOUNTING_GEN` does.

**The rule.** A task's *kernel time* is the time from each entry from user
mode -- a system call, an interrupt or an exception -- to the next return
to user mode, plus all the time of a task with no user mode. Its user time
is its runtime less its kernel time. `stime` reports kernel time, `utime`
the rest, so `utime + stime` stays the runtime the files reported before.

**The state, per processor** (`sched/vtime.rs`), in an array beside
`IN_CALL`, 256 processors of one cache line each, written and read only by
its own processor with interrupts masked:

* `since`: the counter reading at which the running task last entered the
  kernel, or 0 while it is in user mode (a reading of 0 is kept as 1);
* `pending`: the kernel time of the stints that have ended since the last
  settle, in counter ticks, not yet given to a task.

No pointer to a task is kept and nothing is `unsafe`: what is pending goes
to the task a decision finds running, as `queue.current` names it under its
lock (the consultant's condition 5, moot without the pointer).

**The stamps.** In counter ticks, never nanoseconds, so a stamp is one
counter read and no division:

| Where | What |
|---|---|
| x86-64 `ferrix_syscall_entry`, after the fast-path branch, before `general_entry` | `entered`: `since = now`, and should a stint still be open (no return closed it) `pending += now - since` first, so a missed return counts time as kernel time rather than dropping it |
| x86-64 `ferrix_syscall_entry` after `general_entry` returns, and `answer_here`'s `rt_sigreturn` and `enter_compat_after_execve` before their `resume_context`, which never return | `leaving`: `pending += now - since; since = 0` |
| x86-64 `frame_tail`'s general branch, before its `enable_interrupts` | `entered` |
| x86-64, the fast path's `Tail` and `Done` answers after `frame_tail` / `leave` | `leaving`, which finds `since == 0` unless a general switch or the general branch opened a stint |
| `trap::dispatch`, when the frame `came_from_user()`, first and last (after `return_to_user`, `throttle_current` and every other step that can open interrupts) | `entered` and `leaving`: every interrupt and exception from user mode, and the Arm system calls |
| `syscall/process.rs` `run_program`, masked since its test, before `enter_user` and a fork child's `resume_user` | `leaving` |
| `choose_next`, under the queue lock, with the decision's own reading (`timer::now_with_ticks`: one counter read, its nanoseconds for the charge and its ticks for this) | settle: `pending + (now - since)` when `since != 0` to `queue.current`, `pending = 0`, then `since = now` whatever it was: from here the processor is in the kernel, whichever task runs after the switch |
| `current_runtime` (`CLOCK_THREAD_CPUTIME_ID`, a reader's own file) | the same settle, so a thread reading itself counts its open stint |
| the boot task's adoption and each idle task's (`set_current` outside a switch) | the same settle, before `set_current` |
| `Direct::switch`, before `switch_chosen` | `direct_switch`: `pending` (and a stint, should one be open) to the outgoing task, `since = 0`: two loads and two branches when nothing is pending |

`leaving` with `since == 0` does nothing but its load and branch. Every
stamp `debug_assert`s that interrupts are masked.

**The trim (customer, 2026-10-19: "trim first").** The first build read the
counter four times a round trip on the general path and cost it 2.3% (§5).
Two rules took the readings to none there:

* A stint is opened by an entry (an odd `since`) or by a decision (an even
  one). `leaving` reads the counter only to close one an entry opened; the
  span from a call's or a trap's last decision to its return counts as user
  time. A decision here is any scheduling decision on its processor during
  the call or trap: a switch back after it blocked or was preempted, a
  decision that kept it running, its own read of its CPU time
  (`current_runtime` settles). So a
  call that blocks pays its entry's reading and the decision's own, and a
  call that does not block pays two, as before.
* On x86-64 the three native calls that block (`channel_write_read`,
  `object_wait_one`, `port_wait`, the entry's own `vectors_die_in` set)
  take no entry stamp: their time in the kernel up to their decision counts
  as user time, as the fast path's does. A Linux call that blocks (`futex`,
  `read` on a pipe, `epoll_wait`) keeps its entry stamp.

**What it costs the profile.** `stime` is now a lower bound. A
kernel-bound thread that is preempted, or blocks inside its calls, reads the
part of its kernel time after each such decision as user time: on the N150
box under contention, Endfield's threads' `sys%` reads low by that much.
And on x86-64 the three blocking native calls count their time up to their
decision as user time, where AArch64 and ARMv7-A count it as kernel time.
The fix, one counter reading at an even stint's return taken only while
profiling (or behind a boot option), is a backlog row (p2, "Kernel time
after a decision counts as user time").

`choose_next` passes its processor to the settle, which skips the look at
the per-CPU record. Both exclusions are written into L.sched.81 and
SAFETY-MANUAL AoU-34. On AArch64 and ARMv7-A a native call that blocks
still takes the dispatcher's entry stamp, one `CNTVCT` read a direction
(os-07 told, for the DK1 matrix).

**Only the owning processor touches the state.** `account` is also run on
another processor's queue (a wake's `insert`, `charge_running`), and that
caller must not write state the owner writes without a lock. So the
settle is in `choose_next`, `current_runtime` and the adoptions only,
never in `account_with`. The entries that cannot be masked -- a
non-maskable interrupt, a machine check and a debug exception from user
mode, which take the paranoid entry and do not reach `dispatch` -- take no
stamp, so they never write the state under a stamp they interrupted.

**The fast path takes no reading.** The native `channel_write_read` fast
path (`ferrix.fastpath=on`, outside `present`) enters before the entry
stamp and leaves by the frame tail's quiet exit or its `Done` answer,
unstamped, so its time in the kernel counts as user time (the consultant's
ruling: accepted as stated behaviour, written as an exclusion in L.sched.81,
SAFETY-MANUAL AoU-34 and ITEM.md). A call it declines is stamped as any
other, and the frame tail's general branch stamps itself.

**Counting is decided once.** `vtime::init`, in `sched::init` before any
task runs and after the clock is chosen, sets one flag from
`arch::cheap_counter_is_clock` (x86-64: the clock is the TSC; AArch64 and
ARMv7-A: always, `CNTVCT`) and prints `sched    kernel time counted ...` or
`... not counted ...`. Every stamp and every settle reads the flag first.
With the HPET as the clock (x86-64 under TCG: no invariant TSC and no
hypervisor to ask) nothing is counted, `stime` stays 0, and the checks of §4
say their kernel-time cases were skipped; an x86-64 TCG row is no evidence
for L.sched.80-82.

**Read.** `Task::kernel_time()` converts once, at the reader
(`timer::counter_ticks_to_nanos`), and is clamped to the task's runtime,
since the two are charged at different moments. `stat` gives
`stime = kernel / tick` and `utime = runtime / tick - stime`, so the two
add to the runtime in ticks as Linux's `cputime_adjust` makes them; the
process's file sums its live tasks as its runtime already did.

**What a reader on another processor sees** (the consultant's condition
5'). A task's kernel time is given to it at its processor's decisions:
`pending` holds the stints closed since the last one, and the open stint is
in `since`. So a reading from another processor counts the task's kernel
time up to its processor's last decision, and a task alone on a processor
with no tick, making system calls without blocking, may make no decision
for as long as it runs: its `stime` then trails, with no bound, and that
time reads as `utime`. Its own reading (`current_runtime`) settles first and
is exact. The checks read another task's figure only after that task has
exited, its last decision.

**Not changed.** A job's `cpu.stat` keeps its tick split; feeding it the
exact split is a later change (it needs `charge_cpu`'s bandwidth walk made
once per charge, not once per mode). `times()`, `getrusage()` and `wait4`'s
rusage still answer zeros. Both are in the backlog row.

## 3. Switch counts

At every switch (`switch_chosen`, so the general and the direct switch
alike), the outgoing task counts one *involuntary* switch when it is still
runnable -- preempted, yielding (as Linux counts `nivcsw`), or a sleep
`choose_next` could not file (`file_sleeper` refused, the task set runnable
again) -- and one *voluntary* switch otherwise: blocked, asleep or dead. A
decision that keeps the running task counts nothing. A load of the state
and one relaxed store, under the queue lock, on the task's own line.
`status` and `task/<tid>/status` end with `voluntary_ctxt_switches:` and
`nonvoluntary_ctxt_switches:`, last as Linux prints them;
`/proc/<pid>/status` gives the first thread's, as Linux gives the leader's.

## 4. Requirements and checks

H.SCHED.14 (reserved ff18add48): a task's processor time reported split
into kernel time and the rest, its switches counted as voluntary or
involuntary. Under it, in `docs/sysml/15-sched-requirements.sysml`:

* **L.sched.80**, a return to user mode ends kernel time: from a system
  call, and from an interrupt or an exception through `dispatch`. Check (`syscall::check`,
  `check_user_time_is_not_kernel_time`): `USER_KERNEL_TIME_PROGRAM`
  spinning 60,000,000 times after its first entry, after one `getppid`,
  after one page fault, and two of them taking turns on one processor,
  each cut by an interrupt in user mode at least once, are each charged
  under a quarter of their runtime as kernel time. Controls: the system
  call return's `leaving` taken out (x86-64), `dispatch`'s (x86-64 and
  AArch64). A program's first entry, a fork child's `resume_user`, x86-64
  `rt_sigreturn` and a 32-bit `execve` take the same stamp, unchecked, and
  are named in the requirement as not covered (the consultant accepted the
  narrowing of the last three, with the backlog row). The first entry was
  meant to be checked and cannot be: its control, the first entry's
  `leaving` taken out, DID NOT FIRE on the gate (tags `tt3-c1-first-entry`,
  `tt4-c1-first-entry`), because a program's first instruction takes a page
  fault whose return closes the stint microseconds later; what the missing
  stamp costs is those microseconds of start-up counted as kernel time.
* **L.sched.81**, a system call is kernel time. Check: the program making
  50,000 `getppid` calls is charged at least half its runtime as kernel
  time. Controls: x86-64's entry stamp taken out; `dispatch`'s entry stamp
  on AArch64 and ARMv7-A.
* **L.sched.82**, an exception from user mode is kernel time. Check: the
  program mapping 2,048 pages and touching each is charged at least half
  its runtime as kernel time. Control: `dispatch`'s entry stamp taken out
  on x86-64, where system calls do not pass through it.
* **L.sched.83**, switch counts. Check (`check_switches_are_counted`): two
  kernel tasks pinned to one processor yield to each other 20 times each,
  never blocking, and count one voluntary switch (their end) and every
  other switch away from them as involuntary, at least one, the two adding
  to how often each was switched to; a task that sleeps 10 times counts at
  least 10 voluntary and fewer than 10 involuntary. (A first form asked for
  10 involuntary of the 20 yields and failed once on the gate, 9 and 8:
  how many yields switch is the scheduler's choice.)
  Control: the classification inverted.

All run in stage 7, from `stages_check::check_syscalls`, on a processor
other than the checker's where there is one. The existing `/proc` times
check (`fs/procfs/check.rs`, `check_thread_times`) adds `utime` and
`stime`, so it holds; its threads never leave the kernel and now read as
kernel time.

## 5. The round trip

The protocol of `.claude/skills/optimize-ipc-round-trip`: `bench-ipc
--release --accel kvm --smp 1 --alternate <main> --rounds 5 --record`, the
`domain-call` line, on the general path (the default) and with
`--kernel-option ferrix.fastpath=on`, both ABAB against `main`. Figures:
in the landing's commit message and its record under
`docs/hotpaths/results/ipc-round-trip/`.

**Measured** (nazuna, x86_64 under KVM, `--smp 1` pinned to core 11 at
`performance`, SMT sibling 0% busy, host load 2.1 to 2.4; `bench-ipc
--release --accel kvm --smp 1 --arch x86_64 --init <musl busybox>
--alternate ff18add48 --rounds 5 --kernel-option ferrix.checks=skip`,
through `gate.sh`, tags `tt4-bench-general` and `tt4-bench-fast`, on
3afc3e8bf, whose kernel is this landing's; the boot checks skipped on both
sides because stage 10's chardev check failed release boots of `main`
itself twice in three, `docs/backlog/flakes.md` fl-071):

| Path | This tree, p50 a round trip | `main` ff18add48 | Ratio, median of 5 (spread) |
|---|---|---|---|
| general (`ferrix.fastpath` off, the default) | 1,348 to 1,358 ns | 1,318 to 1,328 ns | 1.023 (1.023 to 1.030) |
| fast (`ferrix.fastpath=on`) | 729 to 739 ns | 719 to 729 ns | 1.014 (1.000 to 1.028) |

That was the first build: the general path paid its four counter reads,
about 30 ns a round trip, 7.5 ns a read. **After the trim** (§2, the
customer's "trim first" of 2026-10-19), the same protocol, base ff18add48,
host load 2.8 to 4.4:

| Build | General path, ratio (spread) | Fast path, ratio (spread) | Tags |
|---|---|---|---|
| decision-opened stints read nothing, blocking native calls unstamped | 1.008 (1.008 to 1.015) | 1.000 (0.960 to 1.041) | `tt7-bench-*` |
| and `choose_next`'s settle given its processor | 1.008 (0.993 to 1.015) | -- | `tt9-bench-general` |
| and `general_entry` inlined (this landing) | 1.008 (0.985 to 1.023), 1,318 to 1,348 ns against 1,318 to 1,338 | 1.000 (0.960 to 1.014) | `tt10-bench-*` |
| ablation: this landing with counting switched off in `vtime::init` | 1.031 (1.008 to 1.031) | -- | `tt11-abl-novtime` |

The ablation decides it: the same tree with every stamp a load and a
branch reads 1.031 against the base, higher than the counting build, so
the residual 0.8% is the build-to-build layout band (§5 of the
optimize-ipc-round-trip skill: never-run nops moved a trip by up to 131
cycles), not the counting. The general path's figure is back within that
noise. The `call` line (outside a domain) still reads about 2% above the
base with counting on (5,543 to 5,563 against 5,413 to 5,463 ns) and level
with it off; it is reported, not gated, and its path (a cross-domain
switch with the barrier) is the next to look at if it matters.

Before the trim: The fast path's cost is one 10 ns step of the bench's
counter in three of five rounds and none in one: its few loads and branches
and the switch count's store. Instructions per trip were not counted
(no perf run). The `call` line (outside a domain) read 5,503 to 5,553 ns
against 5,423 to 5,473.

What it adds, by construction: on the fast path, the `direct_switch` look
(two loads, two branches), the quiet exit's `leaving` look (a load and a
branch) and the switch count (a load and a store) a direction, no counter
read; on the general path, two counter reads a direction (entry and exit;
the settle reuses the decision's own reading). The DK1 matrix's general
path gains two `CNTVCT` reads a direction (os-07 to be told).

## 6. A sampling profiler (design only)

What would let a Linux-side tool attribute a thread's time to code:

1. **The sample.** A per-processor profiling deadline folded into the timer
   the scheduler arms (`CpuQueue::arm_timer`): while profiling is on, the
   timer fires at `min(slice end, next sample)`. At the interrupt,
   `dispatch` has the frame: from user mode it records (tid, user
   instruction pointer, user stack pointer, time); from the kernel, (tid,
   kernel instruction pointer, and the system call number if in one). One
   flag load in `dispatch` when off; nothing on the system call path.
2. **The record.** A per-processor ring of fixed 32-byte records,
   allocated when profiling starts, overwritten oldest-first, with a lost
   count. No allocation in the interrupt.
3. **The interface.** First a Ferrix file,
   `/proc/ferrix/profile` (root only): writing `start <hz> [pid]` or
   `stop`, reading drains the rings as text lines
   `cpu tid ns u|k ip [nr]`. A host tool snapshots `/proc/<pid>/maps`
   at the start and end and attributes each user address to a mapping
   and offset -- for Wine, to the PE module mapped there -- and prints a
   flat profile per thread. Later, if a full `perf` is wanted: the
   `perf_event_open` subset `perf record -e cpu-clock` uses (a software
   event, `PERF_SAMPLE_IP | TID | TIME | CPU`, the mmap ring's header and
   `PERF_RECORD_SAMPLE`), against the same rings.
4. **Cost.** None when off. When on, one interrupt a sample a processor:
   under KVM an xAPIC timer write is an exit (about 7 us measured on
   nazuna), so 1 kHz on 4 processors costs under 1% there and less on
   the N150's TSC-deadline timer.
5. **Per-thread system call times** fall out of §2 cheaply: with
   profiling on, the exit stamp adds its span to a per-thread table by
   call number, read as `/proc/<pid>/task/<tid>/syscalls`, which is what
   `strace -c` would show on Linux and the quickest way to find a call that
   costs Ferrix more than Linux.

Estimate: 1 to 3 (5 to 8 points), the interface and host tool included;
the `perf` subset 15 to 25 points more.

## 7. Where it stands

* 2026-10-18: design reviewed by the session's consultant: **OK IF**,
  conditions 1-14 (ledger `reviews.md` line 2406, verdict at 2409); the
  addendum for the built design (no pointer, counting decided once, the
  narrowed L.sched.80, condition 5' and H.SCHED.14 as the parent) at line
  2419. §6 was not reviewed and comes back for its own design review when it
  is built (condition 14).
* Change review of the trimmed build (4dce321c2): **OK IF**, conditions
  1-7, ledger line 2433 (verdict 2443): the evidence, "decision" defined
  and `stime` a lower bound, the profiling cost and its backlog row,
  `settle_on`'s and `direct_switch`'s debug asserts, the message, os-07,
  this list. The checks of §4 now run each kernel-time case up to three
  times, failing only when every run fails, after a host stall charged
  11 of 31 ms of a spin after a `getppid` as kernel time on `tt8-shell-
  ferrousli` (host load 21); the switch case asks for one involuntary
  switch between the two yielders, after `tt8-boot-a32` read 1 and 0.
* Change review of the first build (1acbdcbc4): **OK IF**, conditions 1-6,
  ledger line 2427 (verdict 2429): L.sched.82 narrowed to exceptions,
  control c7 for "a decision that keeps the running task counts none",
  the fail-open skip of a missing test program removed, the message and
  this section. The trimmed build (§2, §5) goes back to it for its own
  review before landing.
* Gated on `gate.sh` (tags `tt8-*` on the trimmed tip; the first build's
  `tt4-*`), every control of §4 FIRED but the first entry's, which cannot
  (§4); the round trip as in §5.
* Next: the sampling profiler (§6), after its own design review;
  `times()`/`getrusage()`/`wait4`'s rusage from the same counts; a job's
  `cpu.stat` from the exact split; the box measurement with
  `tools/common/game/thread-times.sh`.
