# What a block and a wake cost

What a thread pays to block and be woken on Ferrix, against Linux in the same
guest, and the three changes that come out first (2026-11-12, `sys/wake`).

## 1. Why this, and not the system call's floor

Endfield makes about 80,000 system calls a second, and about 17,000 of them are
`futex` and `futex_waitv`, most of them a block or a wake. Its kernel time is
15 ticks a second on Ferrix against 11 on Linux on the same machine. The floor
of a call (`SYSCALL-FLOOR.md`) is about half a percent of one core at that
rate; blocking and waking is the larger part.

## 2. The figures

`wakebench` (`~/.local/share/ferrix/ef-30fps/faultbench/wakebench.rs`, a
static musl program): two threads hand a turn back and forth, each blocking
until the other wakes it. All figures are from a KVM guest on nazuna with
xtask's `-cpu` (no PCID), Linux 7.0 in the same guest, a release desktop
kernel, and `ferrix.checks=skip`. The host load was 12 to 60, and each row
comes from at least two boots.

| us per round trip (processor time a side) | Linux 7.0 | Ferrix |
|---|---|---|
| futex wait and wake, one processor | 1.17-1.43 (0.60-0.72) | 2.15-2.43 (1.10-1.23) |
| `futex_waitv` on two words, one processor | 1.28-1.44 | 2.58-3.98 |
| pipe, one processor | 1.37-1.49 | 2.26-3.59 |
| eventfd behind epoll, one processor | 1.61-1.72 | 3.65-5.49 |
| futex, one processor, 200 other threads parked in futex | unchanged | unchanged |
| futex, processors 0 and 1 | 10.2-12.4 (about 4.5 a side) | 6.1-12.7 |
| epoll, processors 0 and 1 | 11.8-15.4 | 14.5-20.3 |

On two processors, the guest's halt and interrupt cost about 4.5 us a side,
on Linux as well. That swamps everything a kernel does there, so the guest
cannot rank the idle path (§6).

`epollbench` (beside it) runs the same eventfd hand-off, but each thread's set
also holds N registrations that never fire: half pipes, half socketpair ends,
as wineserver's set holds a pipe for every client thread.

| us per round trip, one processor | N = 0 | 16 | 64 | 256 | 512 |
|---|---|---|---|---|---|
| Linux 7.0 | 1.52 | 1.52 | 1.52 | 1.53 | 1.54 |
| Ferrix | 3.59 | 4.12 | 3.89 | 5.07 | 6.26 |

## 3. Where the time goes

**The round trip.** A never-land timeline put TSC stamps at 22 points on
processor 0 along the one-processor futex round trip
(`faultbench/wakediag.patch`). Of the 2.24 us:

| Share | Where |
|---|---|
| ~800 ns | four calls' way in and out (the floor, and the user loop) |
| ~570 ns | two waits' setup: a `Sleeper` (an `Arc`, a `Vec` and a job charge); the word read twice in `list`; `wait_on_any`'s `drained` `Vec`; the `interrupted` look; the push onto the global `SLEEP` queue |
| ~530 ns | two switches: `choose_next` and the eager xsave/xrstor, 260 ns each, not yet split |
| ~490 ns | two wakes: the bucket walk; `sched::wake`; and the entry's drop with `shrink` taking the bucket lock again for nothing |
| ~230 ns | the waits' teardown (`unqueue` from `SLEEP`, `leave`, and `leave`'s `shrink`) |

**One decision per wake.** A futex wake asks for a decision on the waker's
processor whenever the woken task is the first one waiting (`kick_after_wake`),
and that decision never switched (`pick_and_switch` 0.96 times a call, a switch
0.52 times).

**Ablations** (`faultbench/wakeablate.patch`, `ferrix.wakeablate=<mask>`,
boots in the order ABAB):
- No decision for a local wake that should not preempt: the trip falls from
  2.29/2.36 us to 2.16/2.19 us.
- No 1 s recheck filed per wait: within noise.
- A lazy root to idle, and no steal look: within noise on two processors,
  which the guest's halt dominates.

**The box's sampler.** The never-land sampler (d67c959f6) samples at every
interrupt, not by time. An interrupt raised while interrupts are masked is
taken after the next `sti`. All of `block_ending_hold`'s 6,143 samples,
`steal_from`'s 2,272 and `WaitQueue::unqueue`'s 1,455 sit on the single
instruction after their `sti`. So they count interrupts that arrived during a
masked stretch, not time.

The one real-time hot spot is `Epoll::ready` (976 samples, spread over a loop
with interrupts open). Every wait runs
`items.iter_mut().filter(|i| !i.pending && i.always())` over every
registration of the set, to find those every wait must look at. A Wine
process's sets are those of wineserver, with a pipe per client thread, and the
game's own. Linux walks only its ready list. The bench above shows the cost
growing with the set.

## 4. The changes

Each change keeps every observable behaviour and gets its own commit, and each
is measured ABAB in the release guest beside Linux before it is claimed (noise
floor about 0.05 us a trip on one processor). Wake and wait are worth taking
apart, so `sys/wake-futex` is one mechanism per commit.

### 4.1 `sys/wake-futex`: `user/address_wait.rs`, `sched/wait.rs`

**W1. A futex wait blocks on no queue.** `address_wait::sleep` waits on
`&[&SLEEP]`. That is one `WaitQueue` for every futex waiter in the machine,
taken with interrupts masked twice a wait, with an `Arc<Task>` pushed and a
`retain` over every waiter at the end. Nothing ever wakes `SLEEP`: a futex wake
rouses the sleeper's task by name (`Sleeper::rouse`, then `sched::wake`). So
the wait becomes `wait_on_any(&[], ..)`, and `SLEEP` goes. `fs/wake.rs` already
waits on no queue this way when it has no memory to list one.

What made a lost wake impossible was never the queue. It is the pair:
- the waiter marks itself `BLOCKED` and then looks at `woken`;
- the waker `compare_exchange`s `woken` and then reads the state in
  `wake_at_home`.

Today both are read-modify-writes (`AcqRel` on both sides). Neither lock
orders the pair in Rust's model: only the waiter takes `SLEEP`'s, and the
waiter's last look takes no bucket lock. The hardware carries it: a locked
`xchg` on x86-64, `ldrex`/`strex` between `dmb`s on ARMv7-A, and on AArch64
an RMW that the `ldar` after it keeps in order. W1 adds the two fences the
model needs:
- `fence(SeqCst)` in `wait_on_any` after `set_state(BLOCKED)` and before
  the last look;
- `fence(SeqCst)` in `Sleeper::rouse` after a successful `compare_exchange`
  and before `sched::wake`.

The pair is the store-buffering shape, which the model orders by these two
`SeqCst` fences (ledger 9027).

**Every waker of a futex sleeper, and what orders it against the block (WF1):**
- **A futex wake, and a requeue's wake** (`rouse` → `Sleeper::rouse` →
  `sched::wake`): the fence pair above.
- **A signal, a kill or an ending, seen through `interrupted`**
  (`Process::signal_pending`): the signal lock, taken by the poster before
  its wake and by the waiter's look after its `BLOCKED`. The poster's unlock
  then happens before the look, or the look's unlock before the poster's state
  read.
- **The deadline** (the 1 s recheck, or the caller's): the run queue's lock
  and its sleeper set. `choose_next` files the deadline under the lock, and the
  timer's wake takes it out under the same lock.

The waker's fence is in `Sleeper::rouse`, the one waker no shared lock orders.
It is not in `wake_at_home`, which every wake in the kernel pays. Making the
four accesses `SeqCst` would cost nothing on x86-64 and AArch64, but it changes
`sched/task.rs` for every task. It is the alternative if the fences show in
the ABAB figure. The waiter's fence is paid by every `wait_on_any`, poll and
epoll waits included, and is measured with W1.

**The case (WF3).** One waker against one waiter on two processors, for many
rounds, each hand-off bounded at 200 ms, so a lost wake fails on the case's
own line before the 1 s recheck could rescue it. It runs on x86-64 under KVM,
AArch64, and ARMv7-A at two processors. There are two controls:
- (a) `Sleeper::rouse` marks the waiter woken and makes no wake: it must fail.
- (b) the waiter's fence dropped: recorded, FIRED or DID NOT FIRE. The
  `AcqRel` swap orders the store on every architecture built, so it is
  expected not to fire. The fence's evidence is the argument above.

**Requirement (WF6).** `L.user.246` (reserved), parent `H.OBJ.20`: "A wake
that marks a waiter woken and the waiter's block shall be ordered so that the
wake finds the waiter blocked or the waiter's last look finds the mark." Its
units are `user::address_wait::Sleeper::rouse`, `user::address_wait::sleep`
and `sched::wait::WaitQueue::wait_on_any`, and its criterion is the case
above.

`SLEEP` is read nowhere but `sleep` (WF2). No procfs `wchan`, freezer,
hung-task or stall report names it. The 1 s recheck stays, as the defence
against a lost wake it is.

Gains:
- one machine-wide masked lock and an O(all futex waiters) `retain` less per
  wait;
- an `Arc` clone and drop less;
- `wait_on_any`'s `drained` `Vec` becomes empty, so it allocates nothing.

The guest's warm cache hid the global list's cost at 200 waiters. On the box,
the game's threads all share the one lock.

**W2. `shrink` only when the held lock shows it is due.** Every wake (`rouse`)
and every `leave` calls `shrink(bucket)`, which takes the bucket's lock again
to read its length and room. In almost every case there is nothing to give
back. Under the hold the caller already has, the test `len * 4 < room &&
room > MIN_ROOM` decides whether to call `shrink`, which keeps its own test
under its own lock. The bucket's room is given back exactly when it was
before; only the empty lock round-trips go. The test is made on every path
that calls `shrink` today: a wake, a `leave` and a requeue (WF4). The bound in
`MEMORY-AND-TIMING.md` §2.2ad stays true as written: a bucket the held test
finds within four times its live entries, or at `MIN_ROOM`, is within the
bound as it is let go. The `addrwait` line's case 13 already fills a bucket
with 1,024 entries, empties it through `leave`, and requires its room back
at `MIN_ROOM`.

**W3. The word read once when its page is there.** `list` reads the word with
a faulting copy, then again under the bucket lock without a fault. Linux reads
it under the lock first (`get_futex_value_locked`) and faults only if that
fails. W3 does the same: under the lock with no fault first, then the faulting
read with nothing held, and round again only when the page was not there. The
value compared is always the one read under the lock, as now. `L.user.136`
and `L.user.137` hold as written. `L.user.137`'s criterion gains two cases
(WF5):
- a wait on a word whose page is not yet there reads it with nothing held,
  lists on a later round and is woken. On a never-touched object's page the
  first read fault leaves no translation, so the word lists after the second
  read, as on develop, whose loop makes a faulting read every round;
- a wait on a word whose page is there makes no faulting read.

The control leaves the round out.

**W4 (measured, not proposed).** Dropping the 1 s recheck deadline for an
untimed wait gained nothing beyond noise, and it is a defence against a lost
wake. It stays.

### 4.2 `sys/wake-decision`: `sched/mod.rs` (`kick_after_wake`)

A wake made on the woken task's own processor, by the task running there,
that should not preempt it (`CpuQueue::should_preempt` false), does what the
deferred path (`Wake::Sync`) already does. It arms the queue's timer under the
lock and asks for no decision. Today such a wake asks for a decision whenever
the woken task is the first waiting, so that the timer for its slice exists.
`arm_timer` gives the timer without the decision.

A wake that should preempt, a wake onto an idle processor, and a wake from
another processor are unchanged.

The question for review is whether `choose_next`'s pick can differ from
`should_preempt` for a task just woken behind a running one. If it can, the
woken task now waits up to a slice where the decision would have switched to
it at once. Before code, the fair class's `should_preempt` and `pick_next` are
read against each other. A checks case looks for a wake that should preempt
still switching at the waker's next way out, and its control removes the
`should_preempt` test. Expected: about 0.13 us a trip, two decisions.

While in this code, the switch's 260 ns is split by stamps (`choose_next`'s
accounting, the user state). No design follows until it is split.

### 4.3 `sys/epoll-always`: `fs/epoll.rs`

`State` counts the registrations every wait must look at: unhookable,
distrusted (already `distrusted`) or nested. It changes the count wherever one
of the three changes and where a registration is added or taken out, under the
set's lock as the flags are. `ready` walks the set only when the count is not
zero. A set with such registrations is walked as now; one with none (the
common case) costs nothing per registration.

A checks case covers a set with an untrusted registration, an unhookable one
and a nested one, each still reported by a wait with nothing pending. Its
control leaves out one of the count's updates. Expected: the bench's line goes
flat in N, from 6.26 us at 512 registrations back towards 3.6.

How many registrations wineserver's and the game's sets hold during play is
read on the box (`~/.local/share/ferrix/ef-30fps/wake/README`). That says what
it is worth there. Since the second commit on `sys/epoll-ready` an epoll
set's `/proc/<pid>/fdinfo/<fd>` has a `tfd:` line for each registration, as
Linux writes it, so `grep -c tfd: /proc/<pid>/fdinfo/<fd>` counts a set's
registrations exactly.

## 5. Not in these branches

- **The switch's 260 ns**: split first (§4.2).
- **The floor** (`SYSCALL-FLOOR.md`): designed, not built.
- **A sleeper kept with its task**, rather than made per wait (about 75 ns a
  trip): it changes when a job is charged for a waiter, and comes after W1 to
  W3 are measured.
- **The rest of epoll's one-processor gap** (3.6 us against 1.6 with no
  registrations): its timeline, taken after 4.3 (§7), puts about 0.45 us of
  each side's 1.8 us in the wait's looks at the set, about four where Linux
  makes one (a blocking `epoll_wait` calls `Epoll::ready` before it sleeps,
  twice as `wait_on`'s test, and twice after the wake; one look after the
  wake would leave, by estimate, about 0.1 us of it), about 0.27 us in the
  switch (§4.2), and the rest in three calls' floor and the eventfd's read
  and write. A read of an eventfd wakes its writable queue, which an `EPOLLIN`
  registration has hooked as well, so the next wait looks at the file for
  nothing: Linux's `ep_poll_callback` drops a wake whose key shares no event
  with the registration. Neither is designed yet.

## 6. The idle path, on the box

The steal look takes two queue locks for every other processor at each idle
entry. The root is swapped to idle and back. Then there is the way out. The
guest cannot rank these, because the halt dominates them.

`~/.local/share/ferrix/ef-30fps/wake/` has the static benches and one script
that runs them under the box's Ferrix and its inspection Linux. It also has
the never-land branch `diag/ef30-timer-sampler`. That sampler counts unhalted
cycles and delivers the overflow as an NMI, so it samples time inside masked
stretches and never wakes an idle processor. The product owner runs these
when the box is free.

The benches ran on 2026-11-13 on the idle box (no game, the same static
binaries), under Ferrix on develop ca03c0f2e (the futex change in), under
Ferrix on the image before it (`main-341c3f721`) and under the box's Linux
7.0. Median µs a round trip; the files are
`~/.local/share/ferrix/ef-30fps/runs/wake-after`, `wake-before` and
`wake-linux`.

| Round trip | Ferrix with the futex change | Ferrix before | Linux 7.0 |
|---|---:|---:|---:|
| futex, one processor | 6.23 | 7.01 | 3.67 |
| futex, two processors | 9.36 | 9.96 | 6.62 |
| futex waitv, one processor | 7.38 | 8.10 | 4.14 |
| epoll, no idle registration, one processor | 8.71 | 8.80 | 5.00 |
| epoll, 64 idle registrations | 10.35 | 10.59 | 4.96 |
| epoll, 256 idle registrations | 15.64 | 15.59 | 5.08 |
| epoll, 512 idle registrations, one processor | 21.88 | 21.95 | 4.99 |
| epoll, 512 idle registrations, two processors | 18.46 | 18.41 | 8.46 |

W1 is worth 0.78 µs of a futex round trip on the box, 11 %, where the guest
showed 0.07. Ferrix still pays 1.7 times Linux for a futex or an epoll round
trip with nothing registered. The epoll round trip grows by about 26 ns for
every idle registration and Linux's does not grow: at 512 it is 4.4 times
Linux's. With the game running, wineserver holds 1,940 open descriptors and
the game 970, upper bounds on what their epoll sets hold, since Ferrix has
no epoll fdinfo yet. That makes §4.3's change the next one.

## 7. Where it stands

- 2026-11-12: measured, and this design written for the design review.
- 2026-11-12, the design review (ledger 9026; entries 9027, 9037 and 9044):
  - `sys/wake-futex` is OK IF WF1-WF8.
- 2026-11-12 to 13: `sys/wake-futex` is built and lands with this entry
  (code review ledger 9264; the rebases' diff checks 9293 and 9399). Three
  changes: W1, a futex wait blocks on no queue and is ordered against its
  wake by a fence pair, so the global `SLEEP` queue is gone (`L.user.246`);
  W2, an address-wait bucket is shrunk only when its held lock shows room
  to give; W3, the word is read under the bucket lock first and faulted in
  only when its page is absent.
  - W3's finding (ledger 9264, ruling (c)): "exactly once" is not true of
    the faulting read. An untouched object page lists after the second
    faulting read, as on `develop` before the change: an uncommitted object
    page that is read stays uncommitted, so it gets no translation
    (`L.user.137`'s design). That is not a defect, and the check's case says
    "at least once".
  - The figures, release guest under KVM, self-checks skipped, one
    processor's futex round trip in µs (Linux 7.0 in the same guest:
    1.17 to 1.43): W1 measured ABAB at host load 3 to 9, 2.12 to 2.03 and
    2.13 against 2.07, about 0.07 µs; an epoll round trip reads 0.04 to
    0.10 µs slower, within about the noise floor; W2 and W3 are within
    noise. The gain in a guest is small. What W1 removes is a lock every
    futex wait and wake on the machine took, which should show on the box
    with the game's threads; that run is owed (`docs/BACKLOG.md`).
  - Ring 0 grows by 29 lines of code for it (W1 +1, W2 +17, W3 +11, seven
    of W3's a checks-only counter; ledger 9399).
  - `sys/wake-decision` is NOT YET: B1 and B2 come first, the reading of
    `should_preempt` against `choose_next`'s pick written down, then WD1-WD4.
  - `sys/epoll-always` is OK IF WE1-WE4. It is load ring.
- 2026-11-13: the box figures are in §6. `sys/epoll-always` is being built.
- 2026-11-13: `sys/epoll-always` built (branch `sys/epoll-ready`): the set
  counts the registrations every wait looks at, moved under its lock from
  each registration's answer before and after wherever its trust, its hook or
  its presence changes, and a wait walks the registrations only while the
  count is not zero. Release guest under KVM, self-checks skipped, one
  processor, µs a round trip, before and after in the same guest, Linux 7.0
  beside: at N = 0, 3.65-3.78 before and 3.56-3.63 after; at N = 512,
  7.03-7.09 before and 3.56-3.59 after; Linux 1.54-1.59 throughout. A second
  pair, host load 7 to 10: at N = 0, 3.45-3.47 before and 3.53-3.54 after;
  at N = 512, 6.69-6.82 before and 3.58-3.70 after (Linux beside read 1.75
  to 2.4 as the host's load rose to 15, too noisy to rank). The line is flat
  in N; at N = 0 the two are within noise. A wait on a set that holds a
  distrusted file, an unhookable registration or a set still walks every
  registration, as before. Code review: ledger 9492, OK IF EP1-EP6, for this
  commit and the fdinfo one below, which may land with it as a second
  commit.
  - The second commit on `sys/epoll-ready`: an epoll set's fdinfo shows its
    registrations, as Linux's `ep_show_fdinfo` does (design and code review
    ledger 9492).
  - The timeline of what is left: §5, and the never-land patch
    `~/.local/share/ferrix/ef-30fps/faultbench/epdiag.patch`.
