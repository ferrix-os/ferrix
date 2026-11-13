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

### 4.4 `sys/epoll-base`: what an epoll wait costs with nothing registered

After 4.3 the epoll round trip no longer grows with idle registrations, but
it starts higher than Linux's: 3.6 against 1.55 us in the release guest
(N = 0, one processor), 8.7 against 5.0 on the box. A never-land timeline
(`~/.local/share/ferrix/ef-30fps/faultbench/epdiag.patch`, TSC stamps at 29
points on processor 0, output `epdiag-1.out`) splits one side of the trip,
an `epoll_wait` that blocks, then `read` and `write` of an eventfd, about
1.8 us. Figures are ticks of a 4.32 GHz counter per wait, each with about 30
ticks of the stamp itself:

| Stamp | Where | Ticks a wait (calls) | ns |
|---|---|---|---|
| m4 | `ready(limit)` on the way in, nothing to deliver | 376 (0.82) | 87 |
| m9 | `ready(1)` as `wait_on`'s test, false (at the top and as the last look) | 636 (1.62) | 147 |
| m10 | `ready(1)` as the test after the wake, true | 559 (0.82) | 129 |
| m5 | `ready(limit)` after the wake, delivering | 557 (1.00) | 129 |
| m6 | `deliver`: the copy out and `delivered` | 472 (1.00) | 109 |
| m15 | the switch's `choose_next` (§4.2) | 1173 (1.00) | 272 |
| m23 | an eventfd read's wake of its writable queue | 92 (1.00) | 21 |

Calls' entry and exit, the dispatch and the eventfd's own work are the rest.

**What Linux does** (`fs/eventpoll.c`, 6.18):
- `ep_poll` asks `ep_events_available` first: the ready list's emptiness,
  read without the lock (`list_empty_careful`), and a sequence count. Only
  when that says yes does it call `ep_send_events`. Before it sleeps it takes
  `ep->lock`, marks itself interruptible, asks `ep_events_available` again
  under the lock and joins `ep->wq`; after the wake it takes events with no
  second test. A blocking wait makes one real look at the files after its
  wake, and one on the way in only when something is listed.
- `ep_send_events` takes the ready list whole (`ep_start_scan`), polls each
  item, writes each event straight to the caller's buffer
  (`epoll_put_uevent`), and puts a level-triggered item back on the list
  (`ep_deliver_event`). There is no events buffer of its own, and the
  delivery is recorded as it is made, under `ep->mtx`, a sleeping lock held
  across the copies.
- `ep_poll_callback` drops a wake whose key shares no event with the
  registration: `if (pollflags && !(pollflags & epi->event.events))`. An
  eventfd read wakes with `EPOLLOUT` (`eventfd_read`), a pipe read with
  `EPOLLOUT | EPOLLWRNORM`, so an `EPOLLIN` registration on either is not
  listed by them.
- `ep_done_scan` wakes another waiter when the list is not empty after a
  scan. Ferrix does not, and this design does not change that.

What is taken: the lock-free availability test before every real look (cut
1), the key filter (cut 2, done statically per queue rather than per wake,
so that the core's wait queues are not changed), and no buffer of the
wait's own (cut 3). Not taken: the sleeping lock held across the copies out,
since a Ferrix set's lock is a spin lock and a copy to the program can fault.

**Cut 1. One real look after the wake.**

`Epoll::may_report()`, read with no lock, answers whether a look could find
anything:
- the set's queue was woken since the last take: `ReadyList::queue().wakes()`
  against a base the set stores in `take_marks`, read *before*
  `ReadyList::take`. `wake_all_with` bumps the count before it takes its
  waiter lock and after the tell's push, so a base read before a take can
  never count a wake whose number that take missed. (A first draft took
  `ReadyList::tells()` as this term. `Mark::tell` bumps `tells` before it
  pushes, so a base read between the bump and the push counts a tell whose
  number the take misses and whose wake comes later, perhaps before the
  waiter is listed; with `listed.woken` then set, no later tell wakes the
  queue, and the set is deaf until the rescan: the design review's
  finding, ledger 9672.)
- the ready list was told anything since the last take: `tells()` against
  its own base, read the same way. A queue that goes tells its marks
  quietly and wakes nobody, as today, so this term only makes the test say
  yes sooner; a tell counted early costs a look and loses nothing;
- `pending` is not empty, or the walk's count (4.3) is not zero: one atomic
  flag, stored under the set's lock in every section that changes either,
  `take_marks`'s own included (the end of `ready` and `delivered`, `insert`,
  `change`, `take_out`), each store made before the section's bases;
- the once-a-second rescan is due: `rescanned_at` mirrored in an atomic.

`may_report` reads the bases (Acquire), then the flag, then the counts. The
bases are the set's, not the wait's: another thread's wait, `poll` or
`select` on the set's descriptor, and a parent set's look (`Epoll::poll` is
`ready(1)`) all take marks. A base stored by another reader's take is
followed, in the order above, by a flag stored in that take's own section:
whatever that take moved onto `pending` is seen, and what it then found
quiet it dropped. A wake after that take moves the count past the base.

`syscall::epoll::wait` then calls `ready(limit)` only when `may_report()`
says so, and `wait_on`'s test becomes `may_report() || signal_pending()`.
While the walk's count is not zero the test is today's `!ready(1).is_empty()`,
since a distrusted file can change without a tell; which form is used is
decided at every evaluation. The `set.trusted() != trusted` and
`signal_pending()` terms stay. `set.trusted()`, which takes the lock twice a
wait for the recheck's length, reads an atomic mirror of `distrusted == 0`
kept the same way.

A blocking wait then makes the look on the way in only when something is
pending (the level-triggered item just delivered, as on Linux), three cheap
tests where it made three `ready(1)`, and one real look after the wake.
Expected: m9 and m10 go, less the tests, about 0.25 us a side, about 0.5 us
of the trip's 2.05 us gap in the guest. On the box, if it scales as the whole
trip does (8.7 against 3.6), about 1 us of the 3.7.

Why no wake is lost. The waker's path is `WaitQueue::wake_all` on a hooked
queue, then `tell_marks`, then `Mark::tell`. `Mark::tell` bumps `tells`
first, then takes the list's lock to push, and then wakes the set's queue,
which always takes that queue's waiter lock (`wake_all_with`, no lock-free
skip). The waiter lists itself under the same waiter lock before its last
test (`wait_on_any`, as today). So either the waker's waiter-lock section
comes first, and the `tells` bump happens before the waiter's read of it, or
the waiter's comes first, and the waker finds it listed and wakes it. A tell
that makes no wake (`listed.woken` already set) comes after one that did,
since `take` clears the flag. `epoll_ctl`'s changes store the flag under the
set's lock and then wake the set's queue, which is the same argument.
`wait_on_any` and `sched/wait.rs` are not changed. The futex change's fence
pair (4.1, L.user.246) orders a waker that takes no queue lock. It stays
where it is, paid by every wait as now, and is not what orders this pair:
the queue's waiter lock does, as it orders today's `ready(1)` through the
list's lock.

Cases (`fs::epoll_check`), each with a control through `gate.sh control`:
- A blocking wait woken by one write makes one real look after its wake. The
  set counts its real looks for the checks (`scans`, beside `looks`). The
  control puts `ready(1)` back as the test and fires on the count.
- A write that lands while the waiter is held in its first test inside
  `wait_on`, before it lists itself, is reported by that wait well before
  the recheck. The write's wake finds nobody listed, so only the test's
  counts, read after the meet, can see it. The meet is inside `may_report`,
  at its second call (the way in's is the first), since `ready`'s own meet
  is no longer reached by the test. A write after the listing is ended by
  the wake itself, so a meet there shows nothing about the counts (the code
  review's EC1, ledger 9702, after a control at the third call did not
  fire). The control takes both count terms out of `may_report`; the first
  check that catches it is named with its line. The base read before the
  take (the review's finding) is argued, not shown: a meet would have to
  hold the waiter inside the set's spin lock.
- A blocking wait on a set holding a distrusted file that changes without a
  wake is reported well within a second (the case allows 500 ms; the
  untrusted recheck is 5 ms), not by the rescan. The control drops the
  count's branch from the test.
- Each mirror equals its value under the lock at every step of the cases
  (the flag against `pending` and the count, the trust against
  `distrusted`), asserted in the check build.
- A blocking wait on a trusted set whose file changes without a wake (a
  defect the rescan is there for) is reported by the rescan within 1.5 s. The
  control drops the rescan term.

No requirement (load ring; nothing a program sees changes); no ids.

**`fs::wake`'s `LOOKS`.** It is one machine-wide `AtomicU64`, bumped by every
`wait_on` and `Sources::wait` test, three times a blocking epoll wait, so
that `eventfd_check` can say a quiet poll looked at most 12 times. On one
processor it costs a locked add each, about 15 ns a wait. On the box every
processor's waits share its cache line. Taken out of the hot path in cut 1:
counted only while a check arms it (a flag read, never written, by the
waits). `eventfd_check` arms it, disarms it on every way out of the case,
and also requires at least one look, so the control (the arming left out)
fires on that.

**Cut 2. Hooks keyed by direction.**

`Inode::poll_queues` names each queue with the readiness its wakes can
change, *as this file sees it*: `visit(WakeSource, Wakes)`, where `Wakes` is
a set of readable (with priority), writable, hang-up and error. A file that
names one queue names it with all of them, which is today's meaning, so the
single-queue files change only in signature. An epoll registration hooks a
queue only when its `Wakes` meet the registration's events with `EPOLLERR`
and `EPOLLHUP`. `EPOLL_CTL_MOD` to more events makes the registration
unsettled, so its next look hooks what it now needs. Hooks it no longer
needs stay until it goes, which can only cost a wake (fail open to more
wakes, never fewer). `poll` and `select` (`fs::wake::Sources`) may take the
same keys later; not in this cut.

The files naming more than one queue, and their keys:
- eventfd: `readable` for readable, `writable` for writable;
- pipe and FIFO end (`pipe::End`): `readable` for readable and hang-up when
  the end reads, `writable` for writable and error when it writes. An end
  that only reads no longer names `writable`, so a reader's `read` (which
  wakes `writable`) no longer lists the reader's own registration;
- unix socket: `receive.readable` for readable, hang-up and error;
  `peer.writable` for writable; `arrivals` for readable (a listener);
  `receive.writable` and `peer.readable` for nothing this end reports (they
  are the peer's readiness). This holds only if every hang-up and error
  transition wakes `receive.readable` as well: `wake_both` on close does.
  The build reads each wake site in `socket.rs` against it, and names any
  queue that carries a hang-up with that key;
- DRM (`readable`, `card.changed`): both readable, as now.

The 2026-10-17 audit of `poll_queues` is redone for every implementer (23,
all in the Linux personality or `ferrix-vfs`), and `Inode::poll_queues`'
contract is restated: every change `poll` can report wakes a queue named
with a key covering it. Every wake site in `socket.rs`, stream and datagram,
is read against the keys into its module doc. `poll` and `select` keep
hooking every queue a file names, whatever its key; a read-only pipe end
that no longer names `writable` changes what they sleep on, which the build
argues, and the hang-up case runs through `poll` as well. A MOD to more
events makes even a `poll_queues_fixed` registration unsettled, with a case.
The kept edge-triggered over-report gets a BACKLOG row for its own design.

Expected in the bench: m23's tell, about 20 ns a side, and nothing more.
The level-triggered registration is looked at on the next wait anyway, as on
Linux. What it is for: a set whose registrations are edge-triggered or
one-shot, where each read of a pipe or eventfd now makes a look for nothing;
and a thread asleep on a set for `EPOLLIN` while another thread reads the
same pipe or eventfd, which is woken for nothing today. wineserver's request
pipes are read ends registered for `EPOLLIN`. The box's fdinfo shows whether
its sets are edge-triggered. Cut 2 is worth building after cut 1's box
figures, unless that run shows such sets.

Cases: an `EPOLLIN` registration on a pipe's read end, on an eventfd and on
a connected unix socket is not told by a read of the same file (the ready
list's `tells()` does not move across the read), and each is still
reported for data, for a hang-up and for an error. The controls: key the
writable queue for readable (fires on the listing), and key the socket's
`peer.writable` without hang-up while dropping `receive.readable`'s hang-up
(fires on the hang-up case). No requirement.

Not in cut 2: an edge-triggered registration's due report still reads the
file's whole `poll_changes`. A read on a pipe's read end moves it through
the writable queue, so an `EPOLLET | EPOLLIN` registration still readable
reports again where Linux's would not. Keying `poll_changes` the same way
would match Linux. That changes what a program sees, so it gets its own
design and review.

**Cut 3. No buffer of the wait's own.**

`ready` allocates its events `Vec` at every look that copied anything out,
and `deliver` collects the events it wrote into another `Vec` for
`delivered`. Instead, `delivered(&ready[..written])` takes the prefix, and
the events' room is kept in the set as `scratch` is: reserved and charged at
`insert`, taken, filled, given back on every way out of `deliver`, `EFAULT`
included. Nothing is allocated under a lock, and fresh room, when another
wait holds the kept one, is sized by what was looked at, never by
`maxevents`. That is two heap allocations and two frees less a delivering wait. The
timeline does not split them out of m5 and m6 (109 ns with the copy out and
`delivered`'s lock); the estimate is 30 to 60 ns a side, measured ABAB.
`delivered` keeps its own lock hold: recording under the look's hold would
need the copy out under a spin lock, which can fault. One new case, since no
case reaches a failed copy out: with the second event's page unmapped, one
event is delivered and only it recorded, and an edge-triggered registration
whose event was not written stays due; a wholly unmapped buffer is `EFAULT`
with nothing recorded. The control passes the whole room to `delivered`.

**Cut 4. The rescan's due time in counter ticks** (`sys/epoll-clock`,
`fs/epoll.rs` and `fs/epoll_check.rs`, load ring).

A timeline of the built cut 1 (`faultbench/epdiag-cut1.out`) puts most of
the cost of a test that finds nothing in the rescan term's clock read. That
read is `timer::now_nanos`, which is `counter_nanos(counter, hz)`: a counter
read and two 64-bit divisions (a library call before 2026-10). A blocking
wait in the bench reads that clock six times:
- `ready(limit)` twice, once on the way in (the level-triggered item just
  delivered is pending) and once after the wake;
- `may_report` twice, at `wait_on_any`'s first test and at its last look;
- `wait_on_any` twice, for its deadline test and for its slice.

The four in `fs/epoll.rs` only compare the present with when the last rescan
was. They need no nanoseconds.

The set keeps the rescan's due time in the clock's own counter ticks:
- `State::rescan_due` and `Quick::rescan_due` replace `rescanned_at`.
- `ready` reads `arch::counter_now()` (the counter `now_nanos` scales, which
  the personality already reads in `syscall/time.rs`) and passes it to
  `take_marks`.
- `take_marks` rescans when `overflowed || now >= rescan_due`, and then
  sets `rescan_due = rescan_due_after(now, hz)`.
- `may_report`'s last term becomes `arch::counter_now() >= rescan_due`.
- `create` sets the first due time the same way from a counter reading.
- `mirror` and `mirrors_agree` carry `rescan_due` in place of
  `rescanned_at`.

The only arithmetic left is in `rescan_due_after`. It runs at a rescan, at
most once a second and set, and at `create`:

    rescan_due_after(c0, hz) = c0 + hz * (RESCAN_NANOS / 10^9) - (hz / 10^6 + 1)

It saturates at both ends. `RESCAN_NANOS` is a whole number of seconds, which
a `const` assertion in `fs/epoll.rs` holds. `hz` is `arch::counter_hz()`,
read only there: an atomic load on x86-64, and a read of `CNTFRQ` on AArch64
and ARMv7-A. There is no division on any wait's path.

**Why the due time is early by at most `hz / 10^6 + 1` ticks, never late.**
EB3 asks that the trusted recheck always finds the rescan due. In the
argument below the quotients are real numbers, and `floor` is the clock's
rounding.
1. `wait_on_any` files the recheck at its slice reading plus 1 s. The
   scheduler wakes the sleeper at a clock reading no earlier than that.
2. The slice is read after the waiter's own last take, the one whose
   rescan set `rescan_due` from `c0`. So the slice reading is at least
   `n0 = floor(c0 * 10^9 / hz)`.
3. The woken test therefore reads a counter `c` with
   `floor(c * 10^9 / hz) >= n0 + 10^9`. That gives
   `c * 10^9 / hz > c0 * 10^9 / hz - 1 + 10^9`, so `c > c0 + hz - hz / 10^9`.
4. The term is therefore due at that `c` whenever the due time is at most
   `c0 + hz`, less one nanosecond's ticks rounded up.

For `hz` above 1 GHz (the box's TSC, the KVM guest's 4.32 GHz) that is up
to five ticks. A due time of exactly `c0 + hz` would read not yet due at a
recheck that falls in the last nanosecond, and the wait would sleep a second
more. The margin removes that. A margin of one microsecond's ticks plus one
covers a nanosecond's at any counter below 10^15 Hz. A rescan that much
early is one look sooner and loses nothing.

A rescan made after the slice reading moves the due time past the recheck,
and the recheck then finds nothing due. That rescan can be made by the last
look's `ready(1)`, by another thread's wait, by `poll` on the set's
descriptor, or by a parent set's look. That rescan looked at every
registration itself, and only a change after it waits for the next one. It
costs one more slice, as it does today. A change nothing woke then waits at
most about 2 s.

The argument assumes, as now, that the counter is one clock across
processors. `now_nanos` is the same counter scaled, so a skew between
processors would already move `rescanned_at`'s comparison by as much. The
counter's rate cannot change under a due time: `COUNTER_HZ` is stored only
at the clock's start on x86-64, and `CNTFRQ` is set by firmware.

Edges:
- **A counter of zero hertz** makes `rescan_due_after(c0, 0) = c0 - 1`:
  every look is due. With the test always true, a wait would never sleep:
  it would look, find nothing, and test again. That cannot happen once a
  program runs, since the timer knows its rate before the first process,
  check processes included. Today's nanoseconds read zero there, and the
  rescan is then never due.
- **Wrap and saturation.** The due time saturates, as
  `rescanned_at + RESCAN_NANOS` does. The counter wraps no sooner than the
  nanoseconds made from it, since they are the same counter scaled down. The
  clock (`counter_nanos`) saturates past `u64::MAX / 10^9` seconds of
  counter. Past that the nanosecond reasoning above no longer holds, though
  the due time in ticks still follows the counter. That is an argued
  residual: about 580 years at 19.2 MHz, more at any higher rate.
- **The HPET as the counter** (an x86-64 machine without a usable TSC):
  `counter_now` is then three device reads, as `now_nanos`'s is today. Only
  the divisions go.

Nothing else changes:
- The rescan term's meaning and the trusted recheck's 1 s are as before.
- Every check that counts `rescans` still counts them.
- `may_report`'s order (the bases, then the flag, then the counts, then the
  rescan term) is unchanged. `rescan_due` is stored with `Release` in the
  same sections that stored `rescanned_at`, and read with `Acquire`.
- The `look_meet` and `hold_meet` patience loops of the checks keep
  `now_nanos`.

Expected: the divisions of four clock reads less a blocking wait, a side.
What stays in the test that finds nothing:
- a counter read (`rdtsc` on x86-64);
- the four atomic loads of the bases, the flag and the due time;
- the ready list's two counts;
- `test_meet`'s two relaxed loads (ledger 9702 L2), which every
  `may_report` makes in a release build.

The ABAB in the release guest says how much this saves; the guess is 0.05 to
0.15 µs a trip. The N150's cores divide more slowly than nazuna's Zen 5, so
the share there may be larger.

**The case** (`fs::epoll_check::the_rescan_due_time_is_never_late`, run in
the check build on every architecture) calls the product's own
`epoll::rescan_due_after`.
- Rates: 19.2 MHz, 24 MHz, 62.5 MHz, 1 GHz, 14.31818 MHz (the PC's HPET),
  100 MHz (QEMU's HPET), 2.1 GHz, 4.32 GHz, and the machine's own.
- Readings `c0`: 0; 10^9 + 7; the present counter; and `2^62 + 12345`, or
  for a rate below about 250 MHz, two seconds below the clock's saturation.
- With `n(c) = counter_nanos(c, hz)` and `n(c0) + RESCAN_NANOS` added
  checked, it requires:
  - `n(due - 1) < n(c0) + RESCAN_NANOS`: never later than the recheck;
  - `n(due) >= n(c0) + RESCAN_NANOS - 2_000`: no more than 2 µs early.
- A rate of zero must leave the due time at or before its reading.
- A failure prints the rate and the reading.
- It runs once a boot, after the check's second run, and its boot line says
  at how many readings and rates it held: "the rescan's due time in ticks
  fell within 2 us before the recheck at N counter readings and rates".

The rescan case (a trusted set's change that nothing woke is found within
1.5 s) runs on the ticks term. It now also requires that the rescan which
ended its wait set `rescan_due` past the counter read before the wait.

The controls (`gate.sh control --expect`, FIRED (panic)):
- (a) The margin left out (`c0 + hz * s`). At 2.1 GHz from 10^9 + 7, at
  2.1 GHz from 2^62 + 12345 and at 4.32 GHz from 2^62 + 12345 the due time
  is late, and the first of these fires. The list is fixed rather than taken
  from the machine, so it fires the same on x86-64 under KVM and on AArch64.
- (b) A late due time (`c0 + 2 * hz * s`), on x86-64 under KVM. It fires on
  the rescan case's line, "a change nothing woke was not found by the rescan
  of a sleeping wait". That shows the rescan case reaches the ticks term.

No requirement (load ring; nothing a program sees changes); no ids. The
rows are EB8's (ledger 9764, CK1-CK9).

**The same read on the futex and timer-sleep paths (found, not in this
cut).**
- `sched::wait::WaitQueue::wait_on_any` (core) reads `now_nanos` twice a
  round, once for `now >= deadline` and once for the slice
  `now + recheck`. Every futex, `futex_waitv`, epoll, poll and `nanosleep`
  wait pays both. One reading would serve both: the slice then starts a few
  nanoseconds earlier, so the recheck is never later, and the deadline test
  is unchanged. `wait_sliced` has the same pair. Proposed as its own
  branch, `sys/wait-one-read`, since it is core code in `sched/wait.rs`. It
  stays clear of the fence pair (`L.user.246`) and the order around
  `BLOCKED`. Expected: one clock a wait, measured with `wakebench`'s futex
  row.
- `user/address_wait.rs` reads the clock only after a sleep that was not
  woken (the timeout test). A futex round trip does not pay it.
- `nanosleep` (`syscall/time.rs`, `sleep_until`) reads it once a round, for
  the deadline and the time left, which it needs, then pays
  `wait_on_any`'s two.
- The switch (`choose_next`, `timer::now_with_ticks`) converts one counter
  reading a decision. It is needed in nanoseconds for the fair class's
  accounting and the sleeper deadlines. Keeping those in ticks would be a
  scheduler-wide change of unit, which is not proposed.
- `syscall::time::now_nanos` (`clock_gettime`'s system-call path, procfs's
  uptime and loadavg, socket timeouts) still multiplies and divides in 128
  bits, a library call. `timer::now_nanos` gives the same value
  (`counter_nanos`' host test holds it to that formula) with two 64-bit
  divisions. Making the one call the other is a load-ring change with no
  visible effect. It is proposed as a second commit of `sys/epoll-clock`
  if the review wants it there.

None of the cuts touches `user/address_wait.rs` or `sched/wait.rs`.

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
the game 970, upper bounds on what their epoll sets hold, since Ferrix had
no epoll fdinfo then. That made §4.3's change the next one.

The same kit on develop be12356fd (4.3 and the fdinfo in), idle, the same
binaries (`runs/ep1/ferrix-epr-run.txt`):

| Round trip | Ferrix with 4.3 | Ferrix before 4.3 | Linux 7.0 |
|---|---:|---:|---:|
| epoll, no idle registration, one processor | 9.05 | 8.71 | 5.00 |
| epoll, 16 idle registrations | 8.95 | 9.02 | 4.97 |
| epoll, 64 idle registrations | 9.01 | 10.35 | 4.96 |
| epoll, 256 idle registrations | 8.91 | 15.64 | 5.08 |
| epoll, 512 idle registrations, one processor | 8.93 | 21.88 | 4.99 |
| epoll, 512 idle registrations, two processors | 13.16 | 18.46 | 8.46 |

Flat in the registrations on the real machine, as in the guest. With the
game in the world (`runs/ep1/epoll-sets.txt`), the fdinfo shows
wineserver's one set (descriptor 9) holding 468 registrations, of 1,941
descriptors open, and the game's one set holding 1. Which of the 468 are of
the walked kind the fdinfo does not say; the kinds of file it names are
read on the box with

    p=$(pidof wineserver); for t in $(awk '/^tfd:/{print $2}' /proc/$p/fdinfo/9); do readlink /proc/$p/fd/$t; done | sed 's/:.*//' | sort | uniq -c

(a set, the console, `/dev/kmsg` and a render node are the kinds that can
be walked). What is left is the base: 8.9 against 5.0 µs, §4.4.

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
- 2026-11-13: §4.4, `sys/epoll-base`'s three cuts, and their design review
  (ledger 9671-9700): cut 1 OK IF EB1-EB9, with its first test's term
  corrected (the ready list's `tells` read before the take can count a tell
  whose push and wake come after it, and the set is then deaf until the
  rescan: the set queue's wake count is the term); cut 2 OK IF EK1-EK7,
  built after cut 1's box figures unless they show edge-triggered or
  one-shot registrations on pipes, eventfds or unix sockets, or a set more
  than one thread waits on; cut 3 OK IF EV1-EV4.
- 2026-11-13: cut 1 built (branch `sys/epoll-base`): `Epoll::may_report`
  and `Epoll::wait_test` as designed, the bases the set queue's wake count
  and the ready list's tells read before the take, stored after the flag;
  `fs::wake`'s looks counted only while `eventfd_check` arms them. Release
  guest under KVM, self-checks skipped, one processor, µs a round trip
  (before is `develop` with 4.3): before 3.46 to 3.70 in three boots, after
  3.28 to 3.40 in three of six; Linux 7.0 beside 1.58 to 1.60. About 0.2 to 0.3 µs a trip, half of what §4.4 expected. A
  timeline of the built cut (`faultbench/epdiag-cut1.out`) shows why: the
  test that finds nothing costs about 50 ns, most of it the rescan term's
  clock read (`timer::now_nanos`, a 128-bit division), where §4.4 counted
  10. Three of the six after boots read 3.5 to 5.8, with processor time a
  side rising as much; a never-land stamp on every wait longer than 50 ms
  found none, so they are the host's, not lost wakes. The test's clock read
  is the next cut (the rescan's due time kept in counter ticks).
  - Code review: ledger 9702, OK IF EC1-EC6. A control at the third
    `may_report` call, after the listing, did not fire there (the wake
    ends such a wait) and is no evidence; case 2 now meets at the second.
  - Controls, `gate.sh control`, test-boot x86_64 under KVM, each FIRED
    (panic), logs in `~/.local/share/ferrix/logs/queue/`: `eb-ctl-a`
    (the test back to `ready(1)`): "a blocking wait looked at its set more
    than once after its wake"; `eb-ctl-c` (the flag without the walk's
    count): "a sleeping wait did not find a distrusted file's change
    within its recheck"; `eb-ctl-d` (the rescan term false): "a change
    nothing woke was not found by the rescan of a sleeping wait";
    `eb-ctl-e` (the looks never armed): "a poll on a quiet eventfd counted
    no look"; `eb-ctl-f` (both count terms out of `may_report`): caught
    first by the levels case, "a level-triggered set did not report the
    pipe's data with its cookie".
  - The figures' logs, sha256, in
    `~/.local/share/ferrix/ef-30fps/faultbench/epoll-base/`:
    c1-before-1 dfdb64b47d8f9bd65ce689eda26df616508b7b3f839229ae30769855405a8b9f,
    c1-before-2 868941caf68a16113e967114e5d0353cc7d6cc2d2de92133cac07445d1dd7eaa,
    c1-before-3 938e661d5b45fdb07de396a71011ebcc00e36741b6172cdbfb26ec96183dd925,
    c1-after-1 b6108f063436e81b46d7741e8d44c54ca3e10e26b5e939883b1fb21371991601,
    c1-after-2 7e89df9999f3a96a73621c62a20892cc7c5f60352484e75231ba973ca5b8c9a5,
    c1-after-3 88913700083ebd6608f72728c228aa2779e263016bd1b871dc698e4e55ceba16,
    c1-after-4 9e7b42b418ec2e27bc5480c55159281a645ebaeab2a282cafd3237632179f347,
    c1-after-5 2df3beeeb89c0d3084f02e0d4fd9f591603158d4ba7539cfa69bea79f0e9ca29,
    c1-stall 632a6ce89bbf3d145f100b4d53033f9e1db7b19415ac1815af1fa6da91b29e2a,
    c1-linux-1 527f5e3ede2c45813b9ed9b2e93b70a8e63975debdb7fd8a4bd4261c85a191d1;
    the timeline `faultbench/epdiag-cut1.out`
    1c33a93ea7bf2f90412d6f6b3173cd5756f629681301e4819329ef4d84d3840c.
- 2026-11-13: §4.4's cut 4, the rescan's due time in counter ticks
  (`sys/epoll-clock`), written for the design review, with what the same
  read costs on the futex and timer-sleep paths. Design review: ledger
  9764, OK IF CK1-CK9. `sys/wait-one-read` may be a later branch (ledger
  9780, WR1-WR6). The `syscall/time.rs` clock may be a second commit here
  (TN1-TN4).
