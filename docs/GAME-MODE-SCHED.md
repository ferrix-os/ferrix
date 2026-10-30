# Game mode's layer B: what the scheduler is told, and keeps

**Design notes, 2026-10-30, branch `docs/game-mode-sched`. Nothing in them
is built.** They are the design reviews that `docs/GAME-MODE.md` §13 owes
before any code of layer B (the consultant's ledger line 5115, condition
(12)): §5.2 to §5.5 there, the levers F3, F6 and F7, and conditions (6) to
(9). The customer chose tiers T2 and T3 on 2026-10-30
(`docs/backlog/DECISIONS.md`) and not T1, the session service that §8 of
GAME-MODE.md gave the frame mark's right to; §3.1 here names its holder
instead.

Five pieces, in the order they are built, each a review of its own:

| § | Piece | Levers | Ids |
|---|---|---|---|
| 2 | Applied affinity and per-task slice requests | F3, F7 (F6 stays the lag stream's) | L.sched.96-101 (taken over), L.sched.107-109 |
| 3 | The frame mark: its right, its holder, its lifetime, its order | §5.2 points 1 and 2 | H.SCHED.19, L.sched.110-114 |
| 4 | Lends: through a served request, through the wake history, NT sync | §5.3 | L.sched.115-118 |
| 5 | The idle-gap class | §5.5 | L.sched.119-120 |
| 6 | Deadlines from the frame | §5.2 point 3 | L.sched.121-122 |

§7 says how each is measured on the box, §8 holds the questions for the
review, and §9 where each stands.

## 1. What every piece keeps

* **Rules, not figures.** Every requirement states a rule a boot check can
  prove: which task is picked, where it is placed, when a mark or a lend
  ends. None claims a latency, a slice met or a deadline met
  (`docs/certification/MEMORY-AND-TIMING.md` §2.1; ledger 5115 (1)). The
  figures in §7 are product evidence for the customer, not item evidence.
* **Inside eligibility.** Nothing here takes a task out of the fair class
  or makes it ineligible. The virtual time, the weights and the lags are
  EEVDF's as now (`ferrix-sched`). A mark changes *which eligible entity*
  is picked, and an unmarked task's wait is bounded by the kernel, in
  decisions, not by the holder (ledger 5115 (6)).
* **Generic.** The kernel learns no game, no window and no program name.
  It learns that a task is marked, by whom, and what slice it asks for
  (`keep-kernel-slim`, 2026-10-22). Which tasks those are is decided in
  ring 3, by the holder of §3.1.
* **Checked.** Each rule has a boot check in `src/kernel/src/sched/` or a
  test-threads step, and a negative control through
  `gate.sh control --expect` that shows the check fires with the rule
  removed. Arithmetic in `ferrix-sched` has host tests.
* **The core ring.** `sched/**` and `ferrix-sched` are the item's core
  ring (`tools/common/data/certification-item.json`); the system-call
  handlers in `syscall/limits.rs` and the chardev core are the load ring.
  Each piece comes back for its own code review with its ids, rows and
  controls.

## 2. Applied affinity (F3) and per-task slice requests (F7)

### 2.1 F3: the design that exists

`sched_setaffinity` was designed and reviewed on 2026-10-25 and never
built: backlog p1-036, the consultant's **OK IF (D1)-(D12)**, ledger line
4446, with L.sched.96-101 reserved by chardev-sigrace for a branch
`feature/sched-setaffinity` that does not exist on any remote. This design
takes it over unchanged (D1 to D12 are its rules: storage under the home
queue's lock, the lock order in a MEMORY-AND-TIMING section, the mask
intersected with online and the domain, kernel threads refused, every
placement path obeying the new mask, the caller moved before its next user
instruction, the setter waiting for a running target, inheritance across
clone, fork, vfork and exec, Linux's permission, its six ids and its
checks) and changes only the reservation's owner and branch.

What has changed since 2026-10-25, and how F3 uses it:

* **Core quarantine landed** (`docs/CORE-QUARANTINE.md`, L.sched.102-106).
  It added `sched::placeable` to every placer and a re-home of a blocked
  task at its wake (`rehome_blocked`, L.sched.103) and of every queued
  task at a park (L.sched.104). (D5)'s "every placement path re-homes an
  excluded task before it is queued" is the same walk with a wider
  predicate: a home is unusable when it is out of service **or outside the
  task's mask**. So (D5) is built by widening that one predicate at the
  sites §3.1 of CORE-QUARANTINE.md lists, not by a second walk.
* **Quarantine's ruling (g)** said that an applied `sched_setaffinity`
  would make a user's pin an unprivileged veto on parking a processor.
  So F3 adds one rule to (D3): **a mask never vetoes a park.** When no
  processor of a task's mask is in service, the task is placed on the
  in-service processors of its domain, and its stored mask is kept, so
  that it applies again once one of its processors returns (Linux's
  `select_fallback_rq` widens the mask for good and logs it; keeping it is
  the narrower change). `sched_getaffinity` answers stored ∩ in service,
  or the domain's in-service set while that is empty. Kernel threads keep
  their spawn-time pins and their veto (D4).
* **What F3 does not do for game mode.** A mask keeps a task *on* its
  processors; it does not keep anything else *off* them. GAME-MODE's
  "three cores for the game, one for the system" needs every other task
  confined as well. Without cpuset cgroups (out of scope, (D12)), the
  holder of §3.1 sets the masks of the system's Linux processes itself at
  a session's start, writes each mask it changes with the value before to
  its record under `/run` before the write, and restores from the record
  at its end and at its own start (as ledger 5115 (7) asks of freezing).
  Threads made later inherit their creator's mask (D8). Native tasks and
  kernel threads are not confined: the GPU driver is on the path, and the
  per-processor kernel tasks stay where they are (GAME-MODE.md §5.6).
  A cpuset controller is backlog row dg-015.

### 2.2 F7: a slice a task asks for

Today one slice serves every entity of a queue: `slice_for` gives
`TARGET_LATENCY_NS` (3 ms) divided by the number queued, never under
`MIN_SLICE_NS` (`sched/queue.rs`), and `RunQueue::config.slice_ns` is
what every new request is measured with (`enqueue`, `update_curr`'s
renewal, `yield_curr`, `reweigh`, `level`, `hand_over`). Linux 6.12 let a
fair task ask for its own (`sched_attr::sched_runtime`, 0.1 to 100 ms):
a shorter request gives an earlier virtual deadline, so the task is picked
sooner among the eligible and preempts on waking more often, and gets the
same share as before, because its weight is unchanged.

**The rules.**

* **R7.1 (L.sched.107).** Each entity carries a requested slice `r`,
  zero for none. At each new request -- its enqueue, the renewal when its
  request is spent, a yield, a change of weight, a levelling and the
  direct switch's arrival -- its virtual deadline is its virtual runtime
  plus its **effective request** scaled by its weight, where the
  effective request is the queue's slice when `r` is zero and
  `min(r, the queue's slice)` otherwise. `r` is never below
  `MIN_REQUEST_NS`, Linux's 0.1 ms floor, which the setter enforces.
* **R7.2 (L.sched.108).** **No request is longer than the queue's
  slice**, so the lag limit (two of the queue's slices, `lag_limit`), the
  decision bound (`decision_in_ns`) and H.SCHED.2's bound are unchanged.
  This is narrower than Linux, which grants up to 100 ms and widens the
  lag limit with it; a longer request would change a claim the item
  makes, and nothing in game mode wants one.
* **R7.3 (L.sched.109).** A task's request is set only by
  `sched_setattr` on that task, under Linux's permission (D9's), or by the
  frame mark's holder through the mark (§3.3, M6). A clone, fork or vfork
  child starts with none, as Linux 6.12's `__sched_fork` resets
  `se.slice` (to be confirmed against the source in the code review);
  `execve` keeps it, as it keeps every scheduling attribute.

**The system calls** (load ring, `syscall/limits.rs`; no L id, as ledger
3538 and (D10) put the syscall surface):

* `sched_setattr(pid, attr, 0)` with `SCHED_OTHER` or `SCHED_BATCH`:
  `sched_nice` applied as `setpriority` applies it today, and
  `sched_runtime` stored as `r` when it is zero or within 0.1 to 100 ms,
  `EINVAL` outside (Linux's `__checkparam_dl`-free fair check). Every other
  policy, `SCHED_FLAG_RESET_ON_FORK`, the utilisation clamps and any
  unknown flag: `EINVAL`, as `sched_setscheduler` refuses all but
  `SCHED_OTHER` today ("a promise the scheduler has no class to keep").
  `attr.size` handled as Linux's `sched_copy_attr` (`E2BIG` with the size
  written back for a larger unknown tail that is not zero).
* `sched_getattr` answers the policy, the nice value and `sched_runtime`
  as the stored `r`, or the queue's current slice when `r` is zero (as
  Linux answers `se.slice`).

**What F7 does not do alone.** A task woken with negative lag is not
eligible, and waits for the next decision whatever its request
(`docs/SCHED-WAKE-AFFINE.md` §8: the 214 µs-quantised tail). Linux avoids
that with DELAY_ZERO, which is F6, owned by the EEVDF lag stream (p2-149,
ef-sys, ledger 2742) and reviewed there. F7's request helps the woken
task from the decision it becomes eligible at; F6 is what makes that the
wake. The design does not claim F6's gain for F7, and §7 measures F7 with
and without F6 if F6 lands first.

### 2.3 Checks and controls

* **Host tests (`ferrix-sched`):** for each of the six new-request sites,
  the deadline's offset from the virtual runtime is the effective request
  scaled by the weight, `r` zero or not, `r` longer or shorter than the
  queue's slice; `hand_over`'s bit-for-bit test extended with a request;
  the lag limit unchanged by any `r`; two eligible entities, one asking
  for `MIN_REQUEST_NS`, the other none: the short one is picked first.
* **Boot check (`sched/slice_check.rs`, L.sched.107-108):** two spinners
  pinned to one processor, one asking for the minimum, one for nothing,
  and a third asking for twice the queue's slice. At each switch-in read
  through `sched::inspect`, each one's deadline less its virtual runtime
  is its effective request at its weight. Three attempts.
* **Controls:** the renewal measured with the queue's slice
  (`update_curr` ignoring `r`): "a renewed request ignored the task's
  slice"; the cap removed: "a request longer than the queue's slice was
  granted". Both through `gate.sh control --expect`, FIRED.
* **test-threads steps (L.sched.109 and the load surface):**
  `sched_setattr` and `sched_getattr` round trip; `EINVAL` at 99 µs,
  100.001 ms, `SCHED_FIFO` and `RESET_ON_FORK`; a fork child reads the
  queue's slice; an exec keeps the request; another user's task `EPERM`.
  Lines compared with Linux 7.0 on the same processor count (D12's
  practice). Control: the child inherits the request.

F3's checks and controls are (D11)'s, plus a quarantine case: a task
masked to the processor being parked runs elsewhere while it is out and
back on it after (control: the fallback removed, the park refused).

### 2.4 Ids

L.sched.96-101 as (D10) assigns them; L.sched.107-109, parents H.SCHED.2
(107, 108) and H.SCHED.1 (109).

## 3. The frame mark

### 3.1 Who holds the right: sessiond

GAME-MODE.md §8.2 gave the right to the session service of T1, which was
not chosen. The holder this design names is **`sessiond`**, seat0's owner
(`src/user/system/linux/compositor/sessiond`, `docs/AUTH.md` §6.2):

* it is **root**, a unit of `graphical.target` that init starts and
  restarts, and so is outside the user's reach (ledger 698 G2: hyprix and
  yserver run as the user and are reporters, never holders);
* it is **hyprix's parent and its only trusted channel**: descriptors 3 to
  5 are socket pairs no other program of the account can reach, and it is
  already the relay that checks a request it carries and decides nothing
  on the user's word (`docs/SETTINGS-WRITER.md` §12);
* it **places the session's scopes** (`scope.rs`), so it can find the
  game's scope and the path programs' processes itself, as ledger 5115 (8)
  requires, rather than take a pid as given;
* it **ends with the session**: its exit closes the right, and every mark
  ends with it (§3.3, M4), so no failure of the holder leaves a mark
  behind.

Rejected: hyprix (the user's, 698 G2); the GPU driver (a driver given a say
over every process's order); settingsd (absent where
`settings_writer=absent`, and a writer of settings, not the seat's owner);
init (pid 1 grows a policy); a new service (T1's, not chosen).

What `sessiond` gains is ring 3 and outside the item; it is code-reviewed
as any change to it, against these rules:

1. It opens the right at its start (§3.2) and holds it for its lifetime.
2. It reads the **administrator's list**, `/etc/ferrix/game-mode.conf`
   (root's, `0644`, GAME-MODE.md §8.1). Layer B acts only for an
   application named there; the user's list alone grants nothing of it.
3. hyprix asks, on the seat channel, when a window becomes fullscreen and
   focused, naming its client's pid and app id (an X client's through
   yserver). sessiond finds the pid's scope itself in
   `/proc/<pid>/cgroup`, checks it is an `app-*.scope` of the session's
   own user and that the application is on the administrator's list, and
   only then admits the session. What 698 G2 and SETTINGS.md §7.3 say of
   the grant's reach (every program of that user, while the admitted one
   runs) is said on the Settings page.
4. hyprix, yserver and the GPU driver may then report threads that did
   frame work (the render thread that submits, the thread that presents,
   the path's own loops). sessiond marks a reported thread only if it
   finds it, itself, in the admitted game's scope or in one of the path
   programs: hyprix (its own child), yserver (in the session's scope) and
   the GPU driver (by its unit). Anything else is dropped and counted.
5. At focus loss, fullscreen's end, the scope's `populated 0` or the
   user's key it unmarks everything (§8.2 of GAME-MODE.md).
6. It never marks more threads than leave one processor without a marked
   task to run (the 3-of-4 floor of GAME-MODE.md §5.5): a policy on top of
   the kernel's own bound (§3.3, M3), not in place of it (ledger 5115 (6)).

### 3.2 The right: `/dev/ferrix-frame`

A character device of the chardev core (load ring), `0600 root`, made by
the kernel, as `/dev/ferrix-crashes` is. **The open description is the
right**: every mark is given through one and belongs to it.

* `open` needs uid 0 and is refused (`EBUSY`) while another description is
  open, so the right has one holder at a time and a restarted sessiond
  gets it once its predecessor's is closed.
* `ioctl(FRAME_MARK, {tid, request_ns})` marks the task the number names
  in the holder's pid namespace, with a slice request for F7 (zero for
  none, otherwise F7's range); `ioctl(FRAME_UNMARK, {tid})` ends one mark;
  `read` lists the marks the description holds. A kernel thread is
  refused (`EINVAL`); a native task is accepted by its process-table
  number (the GPU driver is native; question Q4).
* A description holds at most `MAX_MARKS` marks (a constant, 64),
  `ENOSPC` past it: the list close walks is bounded.

### 3.3 The rules

* **M1 (L.sched.110), only the right marks.** A task is marked only by a
  `FRAME_MARK` on an open description of `/dev/ferrix-frame`, and only uid
  0 can open one. No task can mark itself, and no other call sets the
  mark.
* **M2 (L.sched.111), the order.** Among a fair queue's **eligible**
  entities, a marked one is picked before an unmarked one; among each,
  the earliest virtual deadline, as now. A woken or arriving marked entity
  that is eligible preempts a running unmarked one. Eligibility itself,
  the virtual time and every lag are computed as without marks: a marked
  entity that has had its share is not eligible and is not picked before
  anything (ledger 5115 (6): never strict priority).
* **M3 (L.sched.112), the kernel's bound for the unmarked.** A queue
  counts its consecutive decisions in which M2 picked a marked entity over
  an eligible unmarked one with an earlier deadline. After `MARK_PASSES`
  such decisions (a constant, 2), its next decision is plain EEVDF, and
  the count starts again. So an eligible unmarked task waits for at most
  `MARK_PASSES` decisions it would have won, each at most one of the
  queue's slices long (R7.2). The bound is in decisions, not in time, and
  holds whatever the holder does. Lent marks (§4) count as marks here.
* **M4 (L.sched.113), the end.** A mark ends, before the call that ends it
  returns:
  (a) when the description that gave it is closed, by `close`, by the
  holder's exit, or by its last reference going; every mark it gave ends
  then, and the close returns only after the last is cleared;
  (b) when the marked task passes `execve`'s point of no return;
  (c) on `FRAME_UNMARK`;
  (d) when the task exits.
  **A clone, fork or vfork child is never born marked**, and its parent's
  mark is not copied (ledger 5115 (7)).
* **M5 (L.sched.114), the mark changes nothing else.** Marking or
  unmarking changes no weight, virtual runtime, lag or placement, and
  moves no task between queues; it re-sorts the entity on its own queue
  under that queue's lock. Placement stays Rule A's (`docs/SCHED-WAKE-
  AFFINE.md` §2) and F3's masks.
* **M6, the request.** A mark given with a request sets F7's `r` for as
  long as it lasts; its end restores the request the task had before
  (R7.3's own, or none). Covered by L.sched.107's check and M4's.

**H.SCHED.19** (item level): *a task the frame mark's holder marks is
picked before unmarked eligible tasks of its queue, an unmarked eligible
task waits for at most a stated number of decisions because of marks, and
no mark outlives its holder's right, its task's exec or the task.*

**What M3 does to H.SCHED.2.** With marks on a queue, an unmarked
entity's positive lag can grow by up to `MARK_PASSES` decisions' worth of
the queue's slices beyond what plain EEVDF allows. H.SCHED.2's statement
gains that sentence, and `RunQueue::check_invariants` holds the wider
bound on queues with a marked entity and the old one on queues without.

### 3.4 How it is built (for the code review)

* `RunQueue` keeps two trees under one virtual time: marked and unmarked.
  `sum` and `load` cover both, so eligibility is unchanged; `choose`
  picks from the marked tree's eligible first, then the unmarked's, then
  the fallback; M3's count lives on the queue. `should_preempt` compares
  across the trees by M2. Host tests prove the lag sums identical to one
  tree's for any sequence (a property test over random operations).
* The task holds its mark as one word (the giver's description id, zero
  for none) written under its home queue's lock (D1's rule); the
  description holds the list of tids it marked. Close walks the list:
  each task looked up, its mark cleared under its queue's lock, the
  entity moved between trees. Exec clears it at the point of no return
  on the exec path that already resets signal handlers.
* Profiles (GAME-MODE.md §8.4, the choice it left here): the device exists
  in every profile and **no program holds it in `max` and `fast`**, which
  carry no desktop and so no sessiond; the safety manual names M1 to M6's
  code deactivated there, and a `max` boot check reads that no mark was
  ever given. A build flag that compiles it out is the alternative
  (question Q3).

### 3.5 Checks and controls

* **Host tests:** M2's order over random queues; M3's count (a marked
  entity with a 0.1 ms request and a full lag against an unmarked eligible
  one: at most `MARK_PASSES` passes, then the unmarked is picked); M5's
  sums.
* **Boot check (`sched/frame_check.rs`, L.sched.111-112, 114):** on one
  processor, a marked spinner with the minimum request and owed lag
  (it slept), and an unmarked spinner: the unmarked one runs within
  `MARK_PASSES` + 1 decisions of becoming eligible (read from the queue's
  pass count and the switch counts), and the marked one is picked first
  while both are eligible. Marking and unmarking leave both lags as
  `inspect` read them before.
* **test-frame (a root user program in `src/tests/`, L.sched.110, 113):**
  open as uid 1000 `EACCES`; a second open `EBUSY`; mark a sleeping
  child and read the mark through `read`; close and read it gone through a
  fresh open; mark, then the child execs: gone; a marked task forks: the
  child unmarked; `MAX_MARKS + 1` gives `ENOSPC`; a kernel thread's number
  `EINVAL`.
* **Controls, each FIRED:** close leaves marks ("a mark outlived its
  giver's close"); exec keeps the mark ("a mark survived exec"); clone
  copies it ("a child was born marked"); M3's count removed ("an unmarked
  task was passed over more than MARK_PASSES times"); the uid check
  removed ("a user opened the frame right").

## 4. Lends: the mark carried through a wait

GAME-MODE.md §5.3. A marked thread that waits on another task to answer
lends that task its mark until the answer. Three kinds, by what the kernel
knows about who will answer.

### 4.1 A served request: exact

When the kernel forwards a caller's request to a ring-3 task and the
caller sleeps for the reply, it knows the server: the chardev core's and
the render core's requests to their drivers (a render node's
`EXECBUFFER2` and fence waits reach the Intel driver this way), and a
native channel call (`channel_write_read`) whose message a server task has
taken. The direct switch (`sched/direct.rs`) already hands the processor
over on a call, but only where nothing else is queued; the lend is on the
general wake path too (ledger 5115's lesser point).

* **L1 (L.sched.115).** While a marked caller waits for the reply to a
  request a server task has taken, that server task is lent the caller's
  mark. The lend ends, before the caller runs again, when the reply is
  written, when the caller's wait ends otherwise (signal, timeout,
  cancel), when the caller's own mark ends (M4), or when the server
  exits. A server holds at most one lend per caller, and lends from at
  most `MAX_LENDS` callers (a constant, 8); a lend past that is not made,
  and counted.

### 4.2 The wake history: measured, for futexes and `futex_waitv`

A futex word names no owner, so the kernel cannot know who will wake a
waiter; it can know who **did** (scx_lavd's graph, measured edge by edge
instead of inferred).

* **L2 (L.sched.116).** Every wake records, in the woken task, the waker
  (its task id) and a streak: incremented when the waker is the same as
  last time, reset to one when not. Written only on wakes of marked tasks,
  so an unmarked machine pays nothing. When a marked task blocks on a
  futex or `futex_waitv` and its recorded waker's streak is at least
  `LEND_STREAK` (a constant, 3), that waker is lent the mark until the
  marked task is woken, by anyone, or its wait ends, or its mark ends, or
  the lendee exits; the same `MAX_LENDS` cap holds.
* **L3 (L.sched.117), no chains.** A task holding only lent marks lends
  nothing: its own blocks make no lend. One hop, from a marked task to the
  task that answers it. A longer chain is a later design with a
  measurement behind it.
* **L4 (L.sched.118).** A lent mark is a mark for M2, M3 and M5, and for
  nothing else: it sets no request (M6), it is not listed by the right's
  `read`, and the holder cannot end it except by ending the lender's mark.

### 4.3 NT sync objects: not yet

Ferrix does not serve `/dev/ntsync` (`docs/WINE.md`: not done); Proton
uses fsync, which §4.2 covers. If Ferrix serves ntsync, a mutex's owner is
known exactly and a marked waiter lends to it as L1 does; that is a
change to this section with its own id, not reserved now.

### 4.4 Checks and controls

* **Boot checks (`sched/lend_check.rs`):** L1: a marked caller's request
  taken by a server task spinning beside an unmarked spinner on one
  processor: the server is picked as a marked task until it replies, then
  as an unmarked one; a signal to the caller ends the lend. L2: a marked
  task woken three times by one waker then blocking on a futex: the waker
  holds the lend until the marked task's wake; a different waker's wake
  ends it too. L3: a lendee blocking makes no lend. Each read through
  `inspect` and the queue's mark counts.
* **Controls, each FIRED:** the reply not ending the lend ("a lend
  outlived its reply"); a wake not ending a futex lend ("a lend outlived
  its waiter's wake"); a lendee's block lending ("a lent mark was lent
  on"); the cap removed ("a task held more than MAX_LENDS lends").

## 5. The idle-gap class

GAME-MODE.md §5.5: work that must run but not now, given a class that
runs where no marked task is runnable. Ferrix already has the weight half
of it: a cgroup with `cpu.idle` 1 runs at the entity weight 3
(`docs/CGROUPS.md`, L.object.185). What it lacks is the order.

* **I1 (L.sched.119).** On a queue with a runnable marked entity, an
  entity whose job is `cpu.idle` is picked only when no other entity on
  the queue is eligible, or by M3's bound, which counts it as unmarked.
  On a queue with no marked entity, nothing changes.
* **I2 (L.sched.120).** An idle-class entity picked under I1 asks for at
  most the queue's slice (R7.2), and for less when §6 gives the queue a
  due time: its request then ends no later than that time.

Nothing is starved for good: I1 is an order among the eligible with M3's
bound under it. What is put in the class is ring 3's (lode's segment
compression, a deferred poll; never the btrfs commit, the audit reader or
the machine-check poll, ledger 5115 (5)), by the holder's writes of
`cpu.idle` on their cgroups, recorded and restored as §2.1's masks.

Checks: a boot check with a marked spinner, an unmarked spinner and an
idle-class spinner on one processor: the idle-class one runs only when the
other two are not eligible, or by the bound. Control: I1 removed ("an
idle-class task ran before an eligible one while a mark was runnable").

**Recommended**: build I1 after §3 measures, and only if the box shows
neighbours in the frame's gaps once the mark is on; the weight alone may
be enough.

## 6. Deadlines from the frame

GAME-MODE.md §5.2 point 3 and GAME-PATH.md's question 3 (may game work
open stage 14 early). This design **does not open stage 14**: it adds no
class, no domain, no admission and no EDF. It gives the mark one more
word, inside the fair class.

* **D1 (L.sched.121).** The holder may give its right a frame period and
  phase (`FRAME_CLOCK`, from hyprix's vblank, which GAME-PATH §4.1 makes
  the real flip time), and the kernel computes the next due time itself,
  so no call is made per frame. Clearing it, or the right's close, ends
  it.
* **D2 (L.sched.122).** While a due time is set, an idle-class entity's
  request ends no later than the next due time (I2), and a marked entity's
  request does not change: all marked tasks share one due time, so it
  orders nothing among them.

So the due time is the idle-gap class's bound and nothing more, until a
machine runs two frames at once. GAME-PATH's G1 and G2 (a `SoftRt` domain
and a fixed-priority class with budgets) stay stage 14's, and question 3
stays the customer's. **Recommended**: no code of §6 until I1 is built
and measured; the ids are reserved so the design is complete.

Checks, when built: the due time computed from period and phase against
the clock at each decision (host test of the arithmetic, boot check that
an idle-class request ends at or before the due time; control: the cap
removed).

## 7. Measuring on the box

On the N150 (`docs/N150.md`), turns taken through
`~/.local/share/ferrix/n150-linux/BOX-HOLDER`. Endfield in the world at
The Hub, as GAME-MODE.md §2.3; Wine's per-second fps channel (never
hyprix's counter); the **lowest second and the spread**, not only the
mean; alternating A/B windows, three rounds; at the played settings
(1280x720, render scale 60%) and **at 640x480, where the processors
co-limit** (39.6 fps, ~93% busy: GAME-MODE.md §3.4).

* **F3:** the game's threads masked to processors 1 to 3 and every other
  Linux process to 0 (a root script standing in for the holder), against
  no masks.
* **F7:** hyprix's, yserver's and the game's render thread's request at
  0.5 ms (`sched_setattr`, a small tool), against none; with and without
  F6 if it has landed.
* **The mark (§3):** after F3 and F7, with sessiond marking.

What the box can show at 720p is limited: the GPU is 99.9% busy, and the
lows, not the mean, are where a CPU lever can show. Each result goes into
GAME-MODE.md §11 and §9 below, with the raw files under
`~/.local/share/ferrix/logs/game-mode/`.

## 8. Questions for the review

* **Q1 (F3).** Does ledger 4446 stand for this design with the
  reservation's owner changed, its (D5) built by widening quarantine's
  predicate, and the fallback rule (a mask never vetoes a park; the stored
  mask kept)?
* **Q2 (F7).** Is the cap (R7.2: no request longer than the queue's
  slice, narrower than Linux's 100 ms) the right price for leaving
  H.SCHED.2 unchanged? Is `sched_setattr` acceptable unprivileged for a
  task's own threads, as Linux has it, given the request only shortens?
* **Q3 (the mark).** sessiond as holder; the open description as the
  right; and for `max` and `fast`, deactivated code (no program holds the
  right) rather than a build flag?
* **Q4 (the mark).** Native tasks markable by their process-table number
  (the GPU driver is on the path), when `sched_setaffinity` refuses them
  (`ESRCH`, (D4))?
* **Q5 (the mark).** M3's bound in decisions with `MARK_PASSES` = 2, and
  H.SCHED.2's statement widened on queues with a mark: acceptable, or
  should a marked queue keep the old bound by a tighter rule?
* **Q6 (lends).** One hop only (L3), the wake history written only for
  marked wakees (L2), and NT sync left until ntsync is served?
* **Q7 (idle-gap, deadlines).** Both designed and reserved, recommended
  built only on the mark's measurement: acceptable as a design now?

## 9. Where it stands

2026-10-30: written in one session (`great-greider-debf91`) from the tree,
GAME-MODE.md, SCHED-WAKE-AFFINE.md, GAME-PATH.md §5, CORE-QUARANTINE.md,
p1-036 and ledger 4446. No code. Ids reserved with this note
(`tools/common/data/requirement-reservations.json`).

| § | Piece | Verdict | Built |
|---|---|---|---|
| 2 | F3 applied affinity, F7 slice requests | awaiting review | no |
| 3 | The frame mark | awaiting review | no |
| 4 | Lends | awaiting review | no |
| 5 | Idle-gap class | awaiting review | no |
| 6 | Deadlines | awaiting review | no |
