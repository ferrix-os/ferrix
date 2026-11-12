# Timer slack, and what a short sleep costs

What a Linux program's short sleep costs on Ferrix, and Linux's timer slack:
how late a sleeping call may be woken, which Ferrix did not have. Measured
and written 2026-11-12 for the Endfield work (a game's threads that sleep in
`pselect6(0, NULL, NULL, NULL, &timeout, NULL)`, Wine's `NtDelayExecution`,
with timeouts of a microsecond or so, in a loop).

## 1. Where it stood

`sleepbench` (`~/.local/share/ferrix/ef-30fps/faultbench/sleepbench.rs`, a
static musl program) times half a second of each call and reads the share of
it the thread was on a processor (`CLOCK_THREAD_CPUTIME_ID`):

| timeout | Linux 7.0, nazuna, native | Ferrix, N150, game beside | Linux 7.0, KVM guest on nazuna | Ferrix, KVM guest on nazuna |
|---|---|---|---|---|
| 1 us pselect | 50.5 us, 1.8 % | 30.6 us, 22.5 % | 58.8 us, 15.0 % | 29.5 us, 18.7 % |
| 10 us | 59.5 us, 1.5 % | 35.9 us, 19.7 % | 68.6 us, 13.1 % | 29.3 us, 18.8 % |
| 50 us | 100.0 us, 0.9 % | 70.7 us, 11.2 % | 111.1 us, 8.4 % | 67.9 us, 8.2 % |
| 100 us | 151.5 us, 0.6 % | 125.0 us, 7.2 % | 166.7 us, 5.2 % | 116.2 us, 4.8 % |
| 1 ms | 1050.4 us, 0.1 % | 1037.5 us, 1.5 % | 1067.1 us, 1.0 % | 1013.3 us, 0.6 % |
| 0 pselect / yield | 0.1 / 0.1 us | 1.0 / 0.7 us | 0.1 / 0.1 us | 0.3 / 0.2 us |

The two guests are the same QEMU, the same `-cpu` (xtask's KVM model,
`tools/common/xtask/src/qemu.rs`), four processors, on the same host; Linux
is Ubuntu's 7.0.0-29 kernel with `sleepbench` as `/init`.

Two things follow.

1. **Linux wakes a relative sleep up to 50 us late on purpose** (its timer
   slack, §2), so its sleeps are longer and fewer. Ferrix wakes at the
   deadline, or 20 us after the call for anything shorter (`MIN_ARM_NS`, the
   shortest interval the scheduler arms its timer for).
2. **A sleep's CPU is not 4 to 7 times Linux's on the same machine.** The
   first comparison set Ferrix on an N150 beside Linux on nazuna. In the same
   guest a 1 us sleep costs Linux 8.8 us of its thread's time and Ferrix
   5.5 us. What Ferrix's 5.5 us is made of is §5.

## 2. What Linux does (read from 6.18's source)

Each task has `timer_slack_ns`, 50,000 for `init` (`init/init_task.c`), and
`default_timer_slack_ns`. A task made by `fork` or `clone` copies its
creator's `timer_slack_ns` (`dup_task_struct`) and takes the creator's
current `timer_slack_ns` as its default (`copy_process`). `execve` changes
neither.

`prctl(PR_SET_TIMERSLACK, n)` (`kernel/sys.c`) sets it to `n` for `n > 0` and
back to the default for `0`, and does nothing for a real-time or deadline
task; `PR_GET_TIMERSLACK` answers it. Moving a task to a real-time policy
sets it to 0 and moving it back restores the default
(`kernel/sched/syscalls.c`).

A timer with slack is a range: it may fire at any instant from its expiry to
its expiry plus the slack, and fires at the end of the range unless an
interrupt for something else comes inside it (`hrtimer_set_expires_range_ns`,
`schedule_hrtimeout_range`).

Which calls get it, and how much:

| call | slack |
|---|---|
| `nanosleep`, `clock_nanosleep` relative **and** absolute (`hrtimer_nanosleep`) | `timer_slack_ns`; `rem` and the restart are taken from the late end |
| `FUTEX_WAIT`, `FUTEX_WAIT_BITSET`, the requeue-PI wait (`futex_setup_timer`) | `timer_slack_ns`, applied again by a restart |
| `futex_waitv`, `FUTEX_LOCK_PI` | none |
| `rt_sigtimedwait` with a nonzero timeout | `timer_slack_ns` |
| `semtimedop` | `timer_slack_ns` |
| `select`, `pselect6`, `poll`, `ppoll`, `epoll_wait`, `epoll_pwait`, `epoll_pwait2` with a nonzero timeout (`select_estimate_accuracy`) | 0 when `timer_slack_ns` is 0; otherwise the larger of `timer_slack_ns` and the time left / 1000 (/ 200 for a nice > 0 task), at most 100 ms |
| any of these with a zero timeout | none: it does not sleep (a zero `nanosleep` does, for the slack) |

## 3. The design

A small typed mechanism, no policy in ring 0 beyond Linux's own rule.

- **The value.** `Thread` gains `timer_slack` and `default_timer_slack`
  (nanoseconds). A process's first thread made by `Thread::leader` starts at
  50,000 for both -- one a native program's `launch::load_native` starts
  too, rather than taking its launcher's -- and a thread made by `fork`,
  `vfork` or `clone` from a running thread takes its creator's slack as both.
  `execve` keeps them.
- **`prctl`.** `PR_SET_TIMERSLACK` (29) and `PR_GET_TIMERSLACK` (30) on the
  calling thread, as `PR_SET_NAME` picks it; a get answers at most
  `ULONG_MAX`.
- **A departure: `SCHED_DEADLINE`.** `sched_setscheduler` refuses every policy
  but `SCHED_OTHER`, but under `schedule=edf` `sched_setattr` takes a
  `SCHED_DEADLINE` reservation (`syscall/limits.rs`, `deadline_setattr`). Linux
  sets a deadline task's slack to 0, restores the default when it leaves the
  policy, and ignores `PR_SET_TIMERSLACK` meanwhile. Here a thread holding a
  reservation keeps its slack and `PR_SET_TIMERSLACK` still sets it, so its
  sleeps are late by 50 us unless it sets 1. Doing Linux's rule is three
  lines in `deadline_setattr` and `slack::prctl` plus a case in an edf build;
  it is a `docs/BACKLOG.md` row, not this landing.
- **The rule.** One type, `syscall::time::slack::Slack`, gives a call's
  slack: `timer_slack` for the sleep-type calls; Linux's estimate for the
  select-type calls (`poll::sleeps_to`); zero for a zero timeout of a
  select-type call or of `rt_sigtimedwait`. A call handed its thread reads
  that thread's; one handed only its process (`poll`, `select`, `futex`,
  `epoll_wait`, `semtimedop`) reads the running task's thread when it is one
  of that process's, and the default otherwise -- a kernel task making the
  call, as a boot check does. `semtimedop` has no case of its own: it takes
  the same `Slack::of_caller(..).sleep()` as `FUTEX_WAIT`, which has.
- **Where it goes.** Each call adds the slack to the deadline it already
  computes and waits to that later instant. What a call writes back is
  Linux's: `nanosleep`'s `rem` and its restart from the late end, `select`'s
  and `pselect6`'s time left from the deadline the program asked for (so zero
  when it timed out). The wait machinery is unchanged: a sleep is still filed
  once, keyed by the instant it wakes at.
- **No coalescing.** A sleep is woken at the end of its range, never inside it
  (see §4).
- **No change to `MIN_ARM_NS`.** A sleep with slack is at least 50 us long
  unless a program asks for less.

## 4. Coalescing: later

Linux wakes a ranged timer early when another interrupt falls inside its
range, which saves interrupts when several sleepers' ranges overlap. Ferrix's
sleeper set already does this for timer objects (`L.object.217`: an arm whose
window has opened leaves with the interrupt that serves the arm before it).
Doing it for sleeping tasks is a field more in the sleeper set's entry and one
more test in `take_next`, about 20 lines with its check. It is left for later
because:

- the set is per processor, so it saves an interrupt only when two sleepers
  on one processor fall within each other's ranges, and the game's sleeping
  threads sleep on processors of their own;
- what it would save cannot be measured before slack itself is on the box,
  and every sleep woken early is one a program sees shorter than Linux's
  would have been when nothing else is due.

## 5. What a sleep's CPU is made of (KVM guest, x86-64, xAPIC)

A diagnostic build that stamps the TSC along one 1 us `pselect6` sleep
(never lands: `~/.local/share/ferrix/ef-30fps/sleep-cost/sleepdiag.patch`,
with the runs beside it),
16,384 sleeps, nanoseconds each:

| step | ns | charged to |
|---|---|---|
| system call entry to `sys_pselect6` (reading the timeout) | 515 | thread |
| `pselect6` to the block (`select`, `wait_for`, `wait_on_any`) | 465 | thread |
| decision out: charge, `vtime`, file the sleeper, pick idle | 470 | idle, but its first 35 |
| **arm the timer for the sleep (one local APIC write, an exit)** | **2,400** | idle |
| address space and user state out | 195 | idle |
| idle: back to `hlt` (reap, steal, look) | 445 | idle |
| interrupt: entry, the timer's handler, EOI | 320 | idle |
| decision in: charge, wake the sleeper, pick | 640 | thread from its `now` |
| **arm the timer for the next sleeper (1.85 s away, an exit)** | **2,040** | thread |
| address space and user state in | 425 | thread |
| back to `wait_on_any`, `pselect6`, the system call's exit | 1,200 | thread |

So under a hypervisor two thirds of a short sleep is two writes of the local
APIC's timer, one to arm the sleep and one to re-arm for whatever is next
once it has fired. Linux makes the same two writes (the `hrtimer` for the
sleep, the reprogramming to the next event after it fires); in this guest its
are as dear (§1: 8.8 us). x2APIC mode (`ferrix.apic=x2apic`) did not make
them cheaper in this guest. On the N150 Ferrix runs in xAPIC mode too, where
a write of the window is an uncached store, not an exit.

The rest, about 3 us charged to the thread and 1 us to idle, is software, each
step a few hundred nanoseconds, none of them a single cause. What would cut it,
with what each costs:

1. **Keep the user address space on a switch to idle** (Linux's lazy TLB):
   about 300 ns a sleep here, more without PCIDs. A scheduler and memory
   change: a root left installed under a task that holds no reference to it
   (`sched::swap_address_space` says why stage 6 did not).
2. **The system call's entry and exit** (515 + ~650 ns after a halt; a zero
   `pselect6` is 0.3 us against Linux's 0.1): the entry work every call
   pays (`vtime`, the filter, `trap::system_call`'s way back). That is the
   IPC round trip's work (`optimize-ipc-round-trip`), not a sleep's.
3. **The idle loop's look before `hlt`** (`steal_work` over every other queue,
   `balance` at every interrupt's exit): about 750 ns of idle time a sleep;
   rate-limiting it as Linux limits new-idle balancing is a scheduler change.
4. **Re-arming only what is due soon after an expiry**: not possible with one
   comparator. A task running in user mode must be covered by an armed timer
   for the next sleeper, however far.

None of these is a timer change, and none is in this note's landing.

## 6. What changes for a program

A sleep that relied on Ferrix's shorter sleeps is woken as late as Linux
would wake it:

- a `pselect6`, `poll` or `nanosleep` of 1 to 50 us wakes about 50 us after
  its expiry instead of at 20 us or its expiry; a sleep loop runs about 1.7
  times fewer rounds a second, and its thread's processor share falls by as
  much;
- a 1 s `poll` wakes up to 1 ms late, a 100 ms one up to 100 us;
- a zero-timeout `select`/`poll`/`epoll_wait` and a zero `rt_sigtimedwait`
  are unchanged; a zero `nanosleep` now sleeps the slack, as on Linux;
- a program that wants the old precision says so with
  `prctl(PR_SET_TIMERSLACK, 1)`, as on Linux.

Timer objects, `timerfd`, `alarm`, interval timers and the kernel's own sleeps
are unchanged.

## 7. Where it stands

- 2026-11-12: this note, for the consultant's design review: **OK IF T1-T9**
  (ledger 8720). The behaviour is the load ring's, which carries no
  requirements (ledger 4262 (b), 7309 (7)), so it has no requirement ids and
  its checks are not item evidence (T1); H.SCHED.5 stays as it is, since the
  item's sleep still wakes no earlier than the instant it is given. The rule
  and its arithmetic, a thread's slack and `prctl`, and every call's wait are
  checked at stage 7 (`syscall/check.rs`,
  `check_sleeping_calls_take_the_threads_timer_slack`), each timed case with a
  lower bound of expiry plus slack and a ceiling of a second.
- Code review: **OK IF U1-U5** (ledger 8759), and its follow-up **OK IF
  U1a** (ledger 8776: the case's other thread set away from the default, so
  a choice of the wrong thread fails it). U1 added the case of a
  call handed only its process taking its running thread's slack
  (`Slack::of_running`); U2 filed ARMv7-A's L.mm.143 failure seen on this
  branch's rows as unexplained, for ef-30fps.
- Measured in the KVM guest (§1's), slack on: a 1 us `pselect6` 60.1 us
  and 9.3 % on a processor (Linux in the same guest 58.8 us, 14.4 %); 10 us
  69.1 us, 8.1 %; 50 us 117.8 us, 4.8 %; 100 us 159.9 us, 3.5 %; 1 ms
  1064.4 us, 0.6 %; `nanosleep` 1 ns 59.3 us, 8.5 %. A short sleep loop now
  runs about half as many rounds a second as before, at about half the
  processor share.
- The cost half (§5) changes no product code: no change there reaches
  Linux's figure in a way this landing could measure, and the guest already
  shows Ferrix below Linux. What would, and what it costs, is §5's list.
