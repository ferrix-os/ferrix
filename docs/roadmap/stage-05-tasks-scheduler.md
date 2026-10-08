# Stage 5 — Tasks and the scheduler ✅

`Task`, kernel stacks, context switch, per-CPU runqueues, the class stack, and
the EEVDF fair class. Scheduling domains exist from the start with one mode
(`Throughput`) implemented; the other two are stage 14, but the domain
abstraction is not retrofitted.

**Done.**

* **The deciding is `src/lib/kernel/sched`**, host-tested, because a scheduler that is
  wrong is wrong in a way nothing on the machine can print. The EEVDF tree,
  the weights, the lag arithmetic and the domain partition are all reachable
  from `cargo test`; what is in `src/kernel/` is the part that needs a machine.
* **Deciding and switching are one operation.** The run queue's lock is taken
  before the decision and released *after* the switch, by whichever context
  ends up running. That is not an optimisation: it is what stops another
  processor picking up the outgoing task in the window between it going back
  on the queue and its registers being saved. `SpinLock::lock_manually` exists
  to say so where the type system cannot.
* **Preemption happens only on the way out of an interrupt.** The timer sets a
  flag and returns; the decision is made once the controller has been told the
  interrupt is done. Switching inside the handler would leave an interrupt in
  service for as long as the next task ran, and a controller still servicing
  one delivers nothing further.
* **The context switch** is one of the two sites in `docs/ASSEMBLY.md`'s
  *Every architecture* table: a function that returns onto a
  different stack from the one it was called on, which Rust cannot express.

**Exit criterion met, and in the boot test on all three architectures.** A
thousand kernel threads, all spawned on one processor so that the only way the
others get any is by taking them, run bounded work to completion and give every
stack back — about 28,000 context switches and 1,000 steals in the runs
recorded when this landed. Then twelve spinners, three on each processor and
one of each three at a different weight, run inside a measured window, and each
task's service is required to stay within EEVDF's own bound of its weighted
share. The bound is not a constant: it is a slice plus the worst overrun the
scheduler actually served, and both numbers are printed, because a bound that
moves is only honest beside what it bounded.

**Three bugs it found, each invisible to the check before it:**

* **An idle processor is never told.** Work appearing on another processor's
  queue after an idle one has halted is invisible to it forever, so a thousand
  tasks ran on one processor with three asleep. Placing a task now wakes the
  idle, and `should_preempt` is not the only reason to: it compares an arrival
  against the fair queue, which the idle task is deliberately not in, so it
  answers false however urgent the arrival.
* **`vmap::free` freed the address before it unmapped the pages**, so another
  processor could be handed an address that was still mapped and have a
  perfectly ordinary allocation refused. The unmapping cannot happen under the
  arena lock — it waits for processors that cannot answer while spinning for
  that lock — so the two steps are separate now, with the address reserved
  across the gap.
* **Every private interrupt the boot core enables is off on every other core**,
  the timer included, because those enable bits are banked. A core whose timer
  is masked in the controller runs, takes inter-processor interrupts, and is
  never preempted: whatever it picks first, it runs forever. The GICv2 driver
  records what was enabled and gives each core the same set, which fixes the
  class rather than the instance.

Two of the three needed more than one processor and work that outlives a
timeslice, which is to say they needed this stage's own test to exist.

**Deferred:** a task that is not a kernel thread, which is stage 6; and the
other two domain modes, which are stage 14.

## Closing the distance to Linux

Stage 5 left the scheduler fair on each processor and naive across them, which
is enough to pass its own exit criterion and not enough to be called a
scheduler. Five things were added afterwards, all of them arithmetic in
`src/lib/kernel/sched` with the kernel supplying the numbers, and each with a check in
the boot test that fails without it.

* **Load tracking.** A decaying average with a 33-millisecond half-life, in the
  shape of Linux's PELT. It measures *weighted demand* rather than occupancy,
  which is the distinction the balancer lives on: a processor is either running
  something or not, so "busy" saturates at one task and says nothing after
  that. Folding in a long stretch costs no more than a short one: past 512
  periods, whatever came before is gone, and the average is set to the level
  held rather than walked there one period at a time. The walk had been done
  under the run queue lock with interrupts masked, three and a half million
  steps for a processor idle for an hour.
* **Placement.** A new task goes where it should rather than where it was
  created — the processor it prefers if that one is idle, any idle processor
  otherwise, the least loaded if none is.
* **Affinity.** `pinned: bool` became a `CpuSet` per task, which is the field
  `sched_setaffinity` wants and is cheaper now than retrofitted. Stealing and
  balancing both honour it. Stage 7's `sched_setaffinity` validates a mask
  and accepts it, but does not yet write it into the task's `CpuSet`.
* **Periodic balancing**, for the case stealing structurally cannot reach:
  every processor busy, one of them much busier.
* **Slice scaling.** The slice is a share of a target latency rather than a
  constant, floored at a minimum granularity. A fixed slice is also a latency
  bound per task, and at a thousand runnable tasks that bound was seconds.

**Four bugs, three of which only a running machine could show.**

* The load average never reached its own fixed point. Two truncating integer
  divisions per step settled a permanently busy processor at 978 of 1024, so
  every processor was compared against a ceiling none could reach.
* Pulling work is useless on a tickless kernel. `arm_timer` deliberately leaves
  a processor alone when nothing is waiting, so an *under*-loaded processor is
  never interrupted and never reaches the balancer to pull anything towards
  itself. The overloaded one is interrupted constantly, precisely because it
  has tasks to switch between — so it is the only one awake to notice, and it
  has to push. Six thousand balance attempts moved nothing before this.
* **A task queued behind a running one did not re-arm its processor's timer.**
  A remote enqueue where `should_preempt` says no left a processor running one
  task forever with others waiting. A hang, not a fairness problem, and the
  cause of an intermittent failure that had been putting the `AArch64` boot
  test down about one run in three.
* Balancing thrashed: the load average is deliberately slow, so moving one task
  does not change it for tens of milliseconds and the balancer kept moving
  more. Eight movable tasks were observed moving 1,262 times. The queue length,
  which updates instantly, is now a brake on the decision the average makes.

Measured on the same boot test: worst-case fairness lag fell from 2,882 to
about 1,000 microseconds, and balance thrash from 1,262 moves to 7.

**Withdrawn, and worth saying why.** Choosing a processor for a task at the
moment it *wakes* — which is what Linux does, and better than placing only at
creation — was implemented and reverted, along with detaching a woken task from
its processor's sleeper set. Each made an already-flaky machine reliably worse;
the second wedged every run. The reason both are harder than they look is the
same: a blocked task is not an unattached one, and "blocked" covers several
states this code does not distinguish. A task can be on a wait queue, in a
sleeper set, part-way into `block` and in neither yet, or in both. A waker that
reasons about one of them moves a task something else still believes it owns.
Naming those states and giving them an order is a change of its own, and the
balancer covers the same ground less promptly in the meantime.

**Found by the review of stages 1–7 on 2026-09-13**, each fixed with a boot
check shown to fail without the fix:

* **The check that a dead task has left its queue could not fail.** It looked
  at a run queue's `previous` under the queue lock, but `finish_switch` empties
  `previous` before it releases that lock. `finish_switch` now asks the
  question itself, at the one moment a dead task leaves its processor for
  good, and records the answer for the invariant check. A kernel broken on
  purpose to leave dead tasks marked queued now ends stage 5 with "a dead task
  is still queued".
* **A dead task could be put back on the run queue.** A task woken after
  marking itself blocked, but before it reached `block`, kept its sleep
  deadline, because only a switch away took it. When the task later exited,
  that switch filed the dead task as a sleeper, and the timer made it runnable
  on a stack the reaper was freeing. `wake` and `exit` now clear the deadline,
  `choose_next` never files a dead task, and `wake_sleepers` wakes only tasks
  still blocked. The check wakes a task in exactly that window and requires
  that it not be filed as a sleeper after it exits.
* **A timer interrupt part-way into a wait lost the task.** `wait_until_deadline`
  marked the task blocked before it set its deadline or joined the waiter
  list, with interrupts on. A switch in between took the task off its run
  queue with no deadline to file it under and no waker able to find it, and
  the machine hung silently. The comment that excused it said every holder of
  the waiter lock masks interrupts; none does. The deadline and the waiter
  entry now come first and `BLOCKED` last, and the way out is the reverse.
  No boot check can hit the window on demand, so it was shown with a
  two-millisecond spin added inside it, locally: the old order hangs stage 5,
  and the new order boots clean with the same spin.
* **A task spawned or woken onto the caller's own processor waited for an
  unrelated interrupt.** The same class as the timer re-arm above, recorded
  as fixed for a spawn or wake onto *another* processor. Onto the caller's
  own, it only set the reschedule flag, which is read on the way out of an
  interrupt, and a system call or kernel thread returns through none. On a
  processor whose timer was stopped, the new task waited until the caller
  blocked or something else happened to interrupt it. The flag now comes
  with the timer armed for the shortest interval, and from inside an
  interrupt the exit re-arms it for the real decision first. The check
  spawns, then wakes, a task onto its own processor while it spins, and
  requires the task to run.
* **A task pulled by periodic balancing was not scheduled.** A pull added a
  task behind a processor's lone running task and asked nothing of it. Its
  timer was stopped, and later wake-ups saw two tasks and did not kick, so
  the pulled task waited for an unrelated interrupt. This was the
  intermittent "a balancing task never started": an anchor-only processor
  took a placement's broadcast, pulled a movable spinner, and never ran it.
  A pull now asks the puller to decide again. The check pulls a task from
  behind a spinner running with interrupts masked, then spins, and requires
  the pulled task to run.

**Four intermittent failures, found a day later and each a real bug.** Stage
5's checks had been failing one boot in three to six on a loaded host, and
had been counted as noise for a day. Probes printed from the worker tasks,
not the checker, and a per-pick trace found them:

* *"a task's stack was never given back"* — the idle loop reaped the whole
  zombie list at once and could be switched out mid-batch when the checker
  was woken onto its processor; the checker then yielded forever waiting for
  stacks the idle task held. The idle task then freed one stack at a time and
  was not switched out while holding one; since 2026-09-19 it frees a batch
  of up to sixteen under one shootdown, still unswitchable while it holds
  them.
* *The thousand-task check taking 20–50 s* — the wait queue's lock was a
  plain ticket lock taken with interrupts on. A worker preempted inside the
  few instructions it is held started a convoy: every finishing worker spun
  its slice away holding a ticket, and each hand-off cost a full round of the
  queue. Two hundred thousand switches to run a thousand tasks. A holder with
  interrupts masked cannot be preempted, so that lock now masks them. Any
  plain spin lock taken from a task with interrupts on is exposed to the same
  thing once contended.
* *"every thread ran on one processor"* and *"a balancing task never
  started"* — a task spawned onto the spawner's own tickless processor, or
  pulled there by the balancer, waited for an interrupt that never came.
* *"a task's service strayed further from its share than EEVDF allows"* —
  not the scheduler. Lag carried *into* the window: a host stall while the
  first spinner ran alone was charged to it as service, EEVDF repaid its
  siblings inside the window, and the check read the repayment as a
  violation. The window now levels every lag when it opens, and its bound is
  a slice plus the sum of the overruns that processor served inside it.

**Preemption is disabled under every task-context spin lock.** The convoy
above was one lock; the kernel had forty more plain ticket locks taken from
tasks with interrupts on, each exposed to the same thing once contended.
Masking interrupts for all of them is the wrong tool, so the scheduler keeps
a per-processor preemption count, raised and lowered by the kernel's
`sync::SpinLock` (a `ferrix_sync::PreemptSpinLock`) for as long as it is
held and while it spins for its ticket, and read on the way out of every
interrupt: a pending reschedule waits until the count is zero, then is made.
A holder must not block, and the scheduler enforces it -- a switch with the
count raised stops the machine (FX-0503) -- so a holder that sleeps is found
by the first boot rather than by a convoy on a loaded host. The run queues'
own locks stay plain, being taken with interrupts masked and handed across a
switch.

**And no shootdown is asked for under one.** A shootdown waits for every
other processor to answer an interrupt, and a holder that asks for one keeps
its lock, contended, with preemption off, for the round trip; a holder with
interrupts masked cannot be answered at all. An audit of every lock in the
kernel on 2026-09-13 found no holder that blocks, and three that shot down:
a shrinking `brk` under the process's state lock, the alarm clock's spawn
under its running flag, and the migration check's spawns under the turn
itself, whose failure path would have waited for the turn it held. Each now
lets go first. The rule enforces itself: the scheduler counts, beside the
preemption count, how much of it *locks* raised, and both flushes assert
that count zero before they take the turn, naming the lock's site when it is
not, and interrupts on wherever they are about to wait for another
processor. A flush that waits for nobody else is exempt on purpose: stage 6's
checks fault with interrupts masked to keep a space installed, and a
copy-on-write fault there retires a page through a shootdown whose set names
only that processor; the first row of this landing found exactly that. The
same row found a real one: a secondary processor enters the idle loop with
the interrupts its hand-over masked, and its first reap frees a stack, a
global flush that waits for every processor; the idle loop now enables
interrupts once at entry. The reaper's own by-hand raise around freeing a
stack, which is a shootdown by design, is not a lock and passes.

**A queue insert charges the running task first.** `CpuQueue::insert`
placed a newcomer before charging the task already running, so a task alone
on a tickless processor -- charged only at its next decision, with an
`exec_start` a hundred milliseconds old -- had all of that billed after the
newcomer was counted, and the newcomer came out owed half of it, past the
placement clamp. Stage 7's first spinner arrived owed 59 ms and ran its
whole loop before the checker could start the second. `insert` and `release`
now charge first, as Linux's `enqueue_entity` calls `update_curr` before it
places; found by stage 7's session with a trace ring, 598 of 600 looped
iterations under KVM, and 2 of 12 whole boots under KVM on the tree before
the charge against 0 of 12 after it. And the check that caught it measures preemption now
-- each program switched out still runnable at the exit of an interrupt that
arrived in its user code, twice -- rather than being switched to twice,
which a program never preempted shows too, or switched away at all, which a
lock released inside its one write with a reschedule pending also does; it
gets three attempts, since a host stall charged to one program as service
lets the other run its whole loop, and prints what each attempt saw.

**Stage 4's contended count judges its overlap over five rounds.** The count
requires two processors' increments to overlap, and an emulator whose host
deschedules whole virtual processors can run the shares one after another
for a round. Each round is judged for the lock's correctness, the first
round that overlaps ends the check, a round that did not is printed with its
shares, and only five rounds without overlap fail it -- the same shape as
stage 7's three-attempt pair check.

**A wait's last look is not preempted (F-69, L.sched.71-72).** A wait lists
the task, marks it `BLOCKED`, makes its last look, and then either marks it
runnable again or blocks (`WaitQueue::wait_sliced`, `wait_on_any`; the fast
path's receive half, `Endpoint::park` and `direct::block_parked`, has the
same shape). Interrupts are on throughout. `choose_next` takes any current
task that is not runnable off its queue, so any switch that lands between
`BLOCKED` and the end of the last look counts as the task's block. The
wakeup is lost when two things happen in turn:

1. A wake drains the task's entry while the task is still runnable, before
   `BLOCKED`. The wake does nothing, because only a `BLOCKED` task is made
   runnable.
2. A switch then comes inside the window. It can be an interrupt's exit, or
   the deferred decision at a `preempt_enable`, which `ready` itself reaches
   when it lets go of the inbox lock it reads under.

The task is then off its run queue and off its waiter list, with no deadline
on a trusting wait. Its message waits in its inbox.

The defect is old. It has stalled a trusting wait for good since 5d5b4f960
(2026-10-01, `write_read` trusts its queue); before that, a recheck bounded
it. It has lost an `END` posted before a park since 0edd644b2 (step 4), until
a message came. Untrusting waits and `wait_on_any` lose only a recheck.

ipc-bench stalled this way on ARMv7-A under QEMU at `--smp 1`, in 2 of 5 runs
on main 9ed9e8428. The processor sat in `wait_then_enable_interrupts` under
`idle_loop` with nothing armed.

Widening the two windows by a build-time spin turned the stall into stage 9's
"wait case 1: a thread blocked in channel_write_read was not woken by a
message within 10 s", in 3 of 3 boots. The same spin with the window masked
passed stage 9 and ran ipc-bench's call series in 3 of 3 boots. Each of those
runs was then cut off by the run's time budget in the domain series, which
the masked spin slows down.

On two processors the drain needs no preemption at all: the peer drains from
the other core.

The window now holds off preemption, not interrupts. The count is raised
before `BLOCKED` and lowered after the last look has set the task runnable
again, or before `block`. In between, an interrupt's exit leaves its request
pending (`preempt_on_irq_exit` returns while the count is raised). A lock
released inside `ready` lowers the count to one, not zero, so it makes no
decision. The request is decided at the lowering:

* If the look found the condition, the task is runnable by then, so the
  switch is a preemption and leaves it queued.
* If it did not, the task is listed (or filed under its deadline), so the
  switch is its block, which is what follows anyway.

Linux closes the same window the other way round: `__schedule(SM_PREEMPT)`
never dequeues a preempted task, whatever its state. That rule was weighed
here and not taken. It would make a fourth task state, blocked and still
queued, that every path would have to know. Those paths are:

* `wake_onto`'s `asleep_at_home`;
* the direct switch's `hand_over`, which takes the peer's run slot and would
  find it held;
* the steal and balance candidates;
* `finish_switch`'s check that no dead task stays queued, which a task
  preempted between `set_state(DEAD)` and its `schedule` would trip;
* the job's load, which `set_state(BLOCKED)` has already let go for a task
  that would go on running.

The raised count changes none of them. The scheduler never sees a blocked
task on a queue except the one running.

**What the raised count leaves as it was:**

* **The callers of `schedule_from` and `choose_next`.** These are the
  interrupt exit, `call_left`, `decide_deferred`, `block`, `sleep_until`,
  `yield_now`, `exit_counted`, the idle loop and `block_parked`. They are
  unchanged, and `choose_next` still detaches by state. Inside the window
  only the first three can be reached, and each already declines while the
  count is raised.
* **The direct switch.** Its `hand_over` runs masked under the home lock
  and detaches the caller it has itself set blocked. It is no preemption,
  and it is untouched.
* **A wake that lands inside the window.** It can come from the other core,
  or from an interrupt on this one. `wake_at_home` finds the task `BLOCKED`
  and sets it runnable, but it does not insert it: the running task is in
  the fair class and reads as queued. `wake_onto`'s `asleep_at_home`
  declines a running task in the same way, so there is no double enqueue.
  The last look's `set_state(RUNNABLE)` is then a swap from runnable to
  runnable, which joins no group twice. If the look did not find the
  condition, `block` switches with the task runnable, and nothing is
  detached. This is main's behaviour before the change too. The count only
  delays the decision the wake asked for (`resched_here`, or a remote
  kick's interrupt exit) to the lowering.
* **A kill or a signal.** `wake_posted` posts its bit, fences and wakes. It
  either finds the task `BLOCKED` and makes it runnable as above, or finds
  it still runnable. In that case the last look's fence-ordered load of the
  bit (`END` through `ready`, or `has_end` in `block_parked`) sees it. That
  look is the one the switch could cut short, and now cannot.
* **Accounting.** Nothing is queued in a new state, so `account_in`, the
  group's load and `cpu.max`'s charge see what they saw before. The task is
  charged for the window as it runs it, as it always was.

**How long preemption is held off.** For a wait, the window covers
`set_state`, the fence (only for a trusting wait) and one call of `ready`.
For the parked block, it covers `Endpoint::park`'s inbox lock, a fence and
one load of `END`.

The worst `ready` is `wait_on_any`'s for `poll` and `epoll_wait`, which asks
each watched file once. A `channel_write_read` reads one inbox under its
lock. Most of each look already ran under spin locks that raised the same
count, so the new hold covers only the parts between those locks.
Interrupts are taken all through it. MEMORY-AND-TIMING §2.2c has the bound.

A `ready` that parks on a `SleepLock` now stops the machine with FX-0503
rather than corrupting the outer wait. A debug build stops it on any
`SleepLock` taken, through `may_park`'s assertion. An uncontended
`SleepLock` never parks, so a quiet boot cannot show a latent one, and
the audit is what shows none exists. It covered every `ready` and every
`poll` that `poll`, `select` and `epoll_wait` reach, and found none that
takes a `SleepLock`, waits for memory, touches user memory or nests a wait.
A `ready` can drop the last reference to an object: a file closed under a
`poll`, and through it a lazily unmounted `Mount` and its filesystem, or a
pidfd's reaped process and its address space. Each of those drops takes
only spin locks and frees without waiting, and the address space's asks
for no shootdown. That is the rule `user/space.rs` already states for the
reaper's preemption window, which drops the same objects.

One thing improves: the console's `poll`, whose line discipline can echo
inside the look, used to be able to wait for room in the console ring
there. That was a nested wait. Inside the look, `may_block` is false, so
it now writes synchronously instead.

**The hold made cheaper (F-69 follow-up, os07-hold, L.sched.71-72).** What
the hold cost was measured on x86-64 under KVM on nazuna, by the long bench
with `perf` on the vCPU (fast path off, core 11 at `performance`; logs under
`logs/os07-hold/`). Instructions per round trip:

* main before F-69 (f42c1ede0): 10,300;
* F-69 (d884add58), two boots: 10,442 and 10,443;
* F-69 with `LAST_LOOK_HOLDS` false and its two cases skipped: 10,299.

So the whole difference is the hold: about 142 instructions a trip. A round
trip has two blocking waits, and each pays a raise of about 14 instructions
and a lowering of about 55. The lowering is `preempt_enable`'s call. The
count comes back to zero, and the deferred decision looks for a request: it
loads the scheduler's start, the processor's record and the request flags,
and reads the interrupt flag. Every blocking wait of a round trip makes that
look and finds nothing.

On ARMv7-A, read from the release build's object code (argued, not run), a
blocking wait pays about 84 instructions for the hold. Four of them are
`dmb ish`, and there are two interrupt mask pairs.

**Cycles and nanoseconds cannot separate the hold from code layout here.**
The same base was built with nops that never run, placed in `wait_sliced`.
With 64 bytes it read +131 cycles a trip; with 256 bytes, -112. Its
instructions moved by 11. So layout alone moves a build by about ±120
cycles (±30 ns) a trip.

F-69's +236 to +279 cycles are its 142 instructions plus a layout shift.
The profile puts the cycles on one locked decrement after `receive_words`
returns. Instruction-cache misses a trip moved with layout too: 1,557 at the
base, 2,137 to 2,276 at F-69, 2,239 with the 64 bytes. Instructions per trip
decide this change, as the optimize-ipc-round-trip skill says they do for
small ones. An ABAB in ns is recorded beside them, not as the verdict.

**The design: a wait that blocks lets the hold go inside the switch's own
mask, and makes no decision of its own** (`LastLook::block`,
`block_ending_hold`). The look did not find the condition, so the task is
listed or filed. The switch it makes next is the decision: `choose_next`
picks under the queue's lock, with every task's state as it stands then.

A request an interrupt left pending is not taken. It stays set for this
processor's next way out: `call_left` at count zero, or an interrupt's exit.
A request always stayed across `block` like this, since no switch clears
it. At most one more decision follows, and it finds nothing.

The count is lowered with interrupts masked, before `require_preemption_on`.
An enable that finds nothing to lower stops the machine with FX-0503, as
`preempt_enable`'s does.

Nothing on this processor can now come between the end of the look and the
switch. Under F-69 an interrupt's exit could switch the task there, as its
block, which was correct. A wake from another processor still can come in
between. It leaves the task runnable and queued, so the switch detaches
nothing and the wait looks again, as before.

`block_ending_hold` is a third entry into `pick_and_switch` and
`choose_next`, beside `schedule_from` and the direct switch. It shares
`schedule_from`'s body (`switch_from`), with the lowering as a parameter, so
that the two cannot drift.

What stays as F-69 made it:

* the window: raised before `BLOCKED`, held through `ready`, with
  `LastLook::hold` and its site in both waits and in `park_for_reply`;
* the branch that finds its condition: its drop lowers the count and makes
  the decision (L.sched.7's promise, which stage 5's case checks);
* the direct switch;
* the `ready` rule.

The count is still never raised across a switch.

**Weighed and not taken:**

* **A raise that notes no site.** Its premise was that the wait's listing
  lock had noted the wait's line just before. It had not: `WaitQueue`'s
  list is an `IrqSpinLock`, which masks interrupts and raises no count. So
  FX-0503 would have named a stale site, and L.sched.21's "naming the site
  that last raised the count" would no longer hold (the consultant's review,
  ledger line 624, H1). It comes back only as a design of its own that
  still names the wait.
* **Folding the raise into `set_state`'s own mask**, the second option
  F-69's author named. It saves nothing on x86-64: the raise is one `xadd`
  without `lock`, and `set_state` masks anyway. On ARMv7-A it would save one
  mask pair a wait, but only through a masked per-CPU add in `arch::percpu`.
  That is new architecture surface for about six instructions, and the same
  holds for the nested mask `block_ending_hold`'s lowering makes on Arm.
  Both are follow-ups, to be measured on the board.
* **A plain `add` and `sub` in place of the `xadd`**, which Zen 5 runs as
  microcode. It read no better.

On x86-64 under KVM, a prototype of this design (a measurement tree, never
landed) read 10,364 instructions a trip. That is 64 above main before F-69
and 78 below F-69: more than half of the hold's instructions. Its cycles,
7,667, are inside the layout band.

On ARMv7-A, read from the object code (argued, not run), a blocking wait
pays about 46 instructions for the hold where F-69 pays 84. It makes one
`dmb ish` instead of four, and one interrupt mask pair is nested inside the
switch's own. A round trip pays two such waits. The DK1 has no fast path,
so it pays them on every trip.

**Still missing against Linux**, none of it on stage 6's path: group scheduling
and bandwidth control, which are stage 13; the real-time classes, which are
stage 14; and NUMA and capacity awareness, which need a topology this kernel
does not yet parse.

---

