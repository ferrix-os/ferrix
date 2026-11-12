# The floor of a Linux system call

What a Linux system call costs Ferrix before it does anything, against
Linux in the same guest, and what can come out (2026-11-12, `sys/floor`).

## 1. The figures

`sysfloor` (`~/.local/share/ferrix/ef-30fps/sys-floor/sysfloor.rs`, a static
musl program) times 200,000 raw `syscall` instructions of each kind after
20,000 untimed, three rounds; the second and third are quoted. All in one
KVM guest on nazuna, four processors, xtask's `-cpu`; Linux 7.0 is Ubuntu's
7.0.0-29 with `sysfloor` as `/init` in the same QEMU and `-cpu`. Ferrix is a
**release** build of develop 1aea9a223.

| ns a call | Linux 7.0 | Ferrix desktop, `checks=skip` | Ferrix present, `checks=skip` | Ferrix desktop, checks run |
|---|---|---|---|---|
| `getppid` | 37 | 99-104 | 105-108 | 134 |
| unknown number (`ENOSYS`) | 33 | 76-79 | 78-82 | 106 |
| `sched_yield` | 89 | 148-155 | 151-158 | 212 |
| `pselect6`, zero timeout | 80 | 224-234 | 239-243 | 314 |

**Like with like.** The box boots its desktop image with `ferrix.checks=skip`
(`n150-linux/box`), so the second column is the box's configuration. A boot
that runs the self-checks also runs the way out's audit
(`sched::work::audit`, which asks the personality's `needs_attention` on
every return): about 40 ns a call. That is why the figures taken under
`test-shell`, which runs the checks, read 130 to 175 ns. Linux's guest has
its default mitigations for this AMD processor. Ferrix's desktop profile has
`mitigations=on`, `ssbd=on_request` and `program_barrier=session`; present
has `ssbd=always`. On this processor neither profile runs `VERW` or the BHB
loop at entry. Host load was 20 to 35, with each figure from one boot, so a
difference under about 5 ns is noise.

## 2. Where the time goes

**Stamps.** A diagnostic build stamps the TSC at 20 points of `getppid`
(`sys-floor/floordiag.patch`, never lands). It ranks the steps, but each
stamp costs about 32 ticks and serialises, so its spans add to more than
the call:
- the seccomp hook, 77 ticks;
- the second decode and the dispatch's way in, 116;
- the lend of the calling thread (`with_current`, `of_task`), 65;
- the early decode, 55;
- the way-out look, 33;
- the two kernel-time stamps, 32 each.

**Ablations.** One kernel takes `ferrix.ablate=<mask>` at boot, so each
piece is left out on a boot of its own (`sys-floor/ablate.patch`, never
lands). The results are 98.7 to 104 ns with nothing left out.

| Left out | `getppid`, ns | Saved |
|---|---|---|
| The seccomp hook (`trap::filter_system_call`) | 90.7 | ~13 |
| The kernel-time stamps (`vtime::entered`/`leaving`) | 91.4 | ~13 |
| The calling thread's lend in the dispatch (`getppid` answered without it) | 90.0 | ~10-14 |
| All three | 66.5 and 73.0 | ~30 |
| ...and the way back's four looks (`copy_poisoned`, `regroup_current`, `throttle_current`, `call_left` with `call_entered`), and the way-out look | 55.6 and 55.5 | ~11 more |
| A one-load decode table instead of `from_x86_64`'s chain | 108.0 | nothing |

The way back's pieces, left out one at a time, read slower, not faster.
`call_entered` and `call_left` are a pair, and leaving out either alone
breaks the processor's in-call mark. Only the figure with all of them out
is meaningful.

**What is left.** With every piece above out, 55 ns against Linux's 37.
The rest is the structure of the dispatch: the core's `Once` and indirect
call to the personality, `answer_as`'s tests before its table, and the
signal table asked before `handle`. Each is a few nanoseconds, none
dominant.

## 3. What Linux does instead (6.18, `arch/x86/entry`, `kernel/entry`)

- **The entry.** `entry_SYSCALL_64` saves the same frame and clears the
  same registers as `ferrix_syscall_stub`.
- **The way in.** `do_syscall_64` → `syscall_enter_from_user_mode` makes
  one load of the task's `syscall_work`. The seccomp, trace, audit and
  emulation bits are all in that word, and the slow path runs only when the
  word is not zero.
- **The call.** A clamped index into `x64_sys_call`'s switch.
- **The way out.** `syscall_exit_to_user_mode` makes one load of the
  thread's work flags. Signals, a reschedule, notify-resume and uprobes are
  all in that word.
- **Kernel time.** In the default configuration, user and system time are
  sampled at the tick, so no time is stamped per call. Exact accounting
  (`VIRT_CPU_ACCOUNTING_GEN`) is for `nohz_full` processors only.

Ferrix asks a separate question for each of these: the seccomp hook through
a registered function, its probe and filtered-thread counts, and the
thread's own bit through a downcast; four looks on the way back; the
pending-work look on the way out; and the two kernel-time stamps
(`L.sched.80`-`82`).

## 4. The design for `sys/floor`

Three changes. Each keeps every observable behaviour and is measured ABAB
against its base before it is claimed.

**S1. One entry-work word on the task.**
- The change:
  - The core's `Task` gains an `entry_work` byte. The personality sets its
    bits where it now sets `Thread::watched`: a filter installed, `TSYNC`
    to another thread, Syscall User Dispatch on. The `PROBE_TASK` probe of
    the checks also sets a bit.
  - The entry reads the byte through the running task's pointer (one load
    after `with_current`). It calls the seccomp hook only when the byte is
    not zero.
- Fail-closed:
  - The bit is on the task, not on a processor. A `TSYNC` to a thread
    running on another processor is seen at that thread's next entry, as
    Linux's `TIF` bit is.
  - A setter that forgets the bit leaves a thread unfiltered. So the check
    that a filtered thread's call is judged stays; it is `L.syscall`'s
    seccomp rows, with a control that leaves the bit out.
- Expected saving: about 13 ns (the hook's ablation).

**S2. The calling thread kept, not downcast.**
- The change: `Task` keeps the pointer the personality's downcast found, as
  the lift core's `Task::keep_thread` already does for `personality=user`.
  `thread::of_task` reads it after the first ask in every build.
- The new `unsafe`: one dereference of the kept pointer. It is valid
  because the task's own `Arc<dyn UserThread>` holds that thread for the
  task's life, and only the downcast's answer is ever stored.
- Expected saving: up to 10 ns. The ablation covers the whole lend and more,
  so the downcast's own share is measured before this is claimed.

**S3. One look on the way back.**
- The change: `call_entered`/`call_left`, `copy_poisoned`,
  `regroup_current`, `throttle_current` and the way-out look each test their
  own word today. They become bits of one per-task or per-processor word,
  tested once. A path whose bit is set runs as before.
- Each bit's writer is listed with the order it is written in, against the
  load that reads it. This is the same argument the pending-work word
  (`sched::work`) already makes for its bits.
- Expected saving: 5 to 10 ns. About 11 ns was measured for all of it
  together.

**Not in this branch.**
- **The kernel-time stamps (S4)** are a policy decision, so they are the
  customer's to make. They cost about 13 ns a call and make a thread's user
  and kernel time exact (`L.sched.80`-`82`, H.SCHED.14). Linux pays nothing
  per call and samples at the tick instead. Keeping them is the default.
  The alternative is sampling at the scheduler's decisions and a periodic
  look, which changes the requirements.
- **A decode table:** measured, no gain.

**What it would reach.** About 65 to 75 ns for `getppid` against Linux's
37 (S1-S3), and about 55 with S4. The remaining tens of nanoseconds are the
dispatch's structure (§2), each a few ns.

`sched_yield`'s extra 60 ns and the zero-timeout `pselect6`'s extra 145 ns
over Linux are their own handlers (a decision; `select`'s preparation), not
the floor. They are listed for the step after.

### 4a. What the design review added (ledger 8968, OK IF F1-F12 for S1+S2)

- **One gate at every entry (F1).** The byte decides the filter's ask
  wherever the filter is asked today:
  - x86-64's `SYSCALL` entry and its `int $0x80` entry;
  - AArch64's and ARMv7-A's trap entries;
  - the lift core's restricted and trap entries.

  The fast path and the way out's `filter_quiet` read the same byte. If
  `filter_quiet`, `QUIET_REGISTERED` and the `ferrix_filter_quiet` hook become
  dead, they go.
- **A fail-closed start (F2).** Every task made with a thread starts with an
  ASK bit set. The personality's filter, on a call whose thread holds no watch
  bit, clears ASK under the seccomp lock. So a missed setter, or a task made
  during a `TSYNC`, costs one extra ask and never an unjudged call. Bare and
  kernel tasks start at 0.
- **The setters (F3).** The bits are written only in `Thread::set_watch` and
  in the probe's arm and disarm, by `fetch_or` and `fetch_and` with release,
  under the state's lock. A `TSYNC` reaches each target thread through that
  thread's task. `Thread::watched` stays the truth; the byte is its summary.
- **Inheritance cases (F4):**
  - clone, fork and vfork of a filtered thread;
  - an `execve` that keeps its filter;
  - `launch::load_native`'s children;
  - a `TSYNC` to a thread running on another processor, judged from its next
    entry, as on Linux. This is stated in `SECCOMP.md`.
- **S2's unsafe moves into the core behind a sound API (F5).** The API is
  `Task::thread_as::<T: Any>() -> Option<&T>`. It keeps the first successful
  downcast's pointer together with its `TypeId`, and answers from it only
  for the same type. The one dereference is in the core, and its SAFETY
  names its obligation. The lift's `keep_thread`/`thread_kept` and
  `of_task_known` move onto it, and `Task.thread` gets no setter.
- **Checks (F6):**
  - the entry gate counts its asks: exactly once per call with the byte
    set, never without it;
  - the personality's setters and ASK clearing;
  - `thread_as` for its own type, another type, and a bare task.
- **Requirements (F7).** H.TRAP.16 and L.trap.7 are amended to "every call
  of a task whose entry work is set". There are three new ids, reserved
  first: an L.trap for the gate, an L.syscall for the personality's side,
  and an L.sched for `thread_as`.
- **Controls (F8):**
  - `TSYNC`'s setter left out, with ASK's start removed;
  - ASK's start removed;
  - the entry asking regardless of the byte;
  - `thread_as` without its `TypeId` test;
  - the entry never asking.
- **Rows (F9),** including `test-seccomp`, an `ipc_fastpath=on` boot and
  the `personality=user` rows.
- **Ring 0 (F11).** Ring 0 may rise by S1 and S2's core lines, net of F1's
  removals, up to 120 lines of count A in a pair.
- **S3 is not yet approved (F12).** It needs its own design first: a table
  of each bit's writer, its lock or ordering, the reader's load and what a
  stale read costs.

The expected saving is unchanged for S1, 13 ns. For S2 the saving is at most
the lend's, about 10 ns. F2's ASK makes a thread's first call after a
`TSYNC`-free start one extra ask, once.

## 5. Where it stands

**Designed, not built.** S1 to S4 are a design only; no code was written,
no requirement id is reserved, and nothing here is evidence.

- 2026-11-12: measured, and this design written for the consultant's
  design review: S1+S2 **OK IF F1-F12** (ledger 8968); S3 **not yet**, its
  writer table owed first (F12).
- 2026-11-12, the product owner: **not built now**, and S4 not put to the
  customer now. The game Endfield makes about 80,000 system calls a second
  (the sampler's counts: `read` 22k, `futex` 10k, `futex_waitv` 7k,
  `rt_sigprocmask` 6.5k, `clock_gettime` 4.5k, epoll 4.5k), so the floor's
  63 ns over Linux is about 5 ms of processor time a second, half a percent
  of one core, against a day's work in the seccomp gate and twelve
  conditions. Its kernel time is blocking and waking instead
  (`sys/wake`). Whoever takes this up starts from §4 and §4a, and has the
  review re-read against the tree of that day.
