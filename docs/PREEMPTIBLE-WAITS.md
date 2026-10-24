# Waiting for a kernel lock without holding the processor (F-95)

Design for review, 2026-10-24. Finding F-95, Major (consultant ledger line
3822): "woken ring-3 tasks are kept off every processor for 1-3 s under a
saturating ring-3 workload". The core ring is touched (`src/lib/kernel/sync`,
`src/kernel/src/user/**`), so this design is reviewed before any code (I4).

## 1. What was measured

### 1.1 The box (N150, run E, 2026-10-24)

Endfield was starting and loading its map. 10 ms sleep canaries, one pinned
to each core plus one unpinned:

- All cores woke late in the same second: CPUs 0, 1 and 3 by 0.91 s, the
  unpinned canary by 3.18 s.
- The power driver's loop ran 2.85 s late.
- hyprix drew no frame for 6 s.
- `/proc` reads of other processes took 450-865 ms.
- `getppid` stayed fast, at worst 3.4 ms.
- The kernel's 5 s timer lines kept time.

### 1.2 Reproduced without the game

The program is `stallrep` (`~/.local/share/ferrix/sched-wake/stallrep.c`),
run as init on two guests:

- develop f05996a79 (release) under KVM: `--smp 4`, 4 GiB, vCPUs pinned to
  host processors 20-23, `ferrix.checks=skip`;
- Linux 7.0 in the same QEMU shape, as a baseline.

The canaries are 10 ms sleepers, one pinned to each processor and one
unpinned, each in its own process. Each 15 s phase adds one load process:

| phase | load |
|---|---|
| hog16 | 16 spinning threads |
| futex8 / pipe8 | 8 ping-pong pairs |
| fault8 | 8 threads: map 32 MiB, touch every page, unmap |
| wine8 | 8 threads: mprotect/touch/`MADV_DONTNEED` inside one 2.6 GiB `PROT_NONE` reservation |
| mixed | all of the above at once |
| bigunmap | 6 fault8 threads (8 MiB) plus one that maps, fills and unmaps 1 GiB |
| forkbig | 1 GiB touched, 6 fault8 threads, and the main thread forking a child that exits at once, in a loop |

**What these logs are.** Every Ferrix log's header reads f05996a79, base and
prototype alike: the prototype was a local patch on that hash. So they are
the consultant's I1, diagnosis only, and evidence for nothing the fix will
claim; `test-stall` on the landing hashes is. The host, nazuna, carried a
load of 30 to 60 on 24 threads from other sessions throughout, and its noise
alone reaches 117-199 ms in a phase with no load at all (`base2`'s quiet
phase; Linux's quiet2 98 ms in `linux1`). Runs marked nice 10 ran QEMU at
nice 10, which makes that noise worse.

Worst canary lateness per phase, from each log under
`~/.local/share/ferrix/sched-wake/` (the logs named; Ferrix and Linux run
back to back on host processors 20-23):

| phase | Ferrix develop: nice 10 (`base2`, `diag1`) | nice 0 (`diag3`, `baseq1`, `baseh1`) | Linux 7.0 (`linux1`, `linux2`, `linuxh1`) |
|---|---|---|---|
| quiet | 6-199 ms | 6-42 ms | 9-18 ms |
| hog16 | 2-326 ms | 19-334 ms | 10-48 ms |
| futex8 | 514-1,674 ms | 23-326 ms | 6-46 ms |
| pipe8 | 241-1,207 ms | 32-41 ms | 7-51 ms |
| fault8 | 76-1,532 ms | 305-316 ms | 22-56 ms |
| wine8 | 285-497 ms | 37-280 ms | 12-18 ms |
| mixed | 89-646 ms | 85-545 ms | 12-151 ms |
| quiet2 | 64-1,666 ms | 19-75 ms | 21-98 ms |
| bigunmap | | 157 ms (p99 52-60) | 16 ms |
| forkbig | | **190,656 ms**: 128 s on cpu1 and cpu2, 190 s on cpu3, 1.0-1.2 s on the others | 21 ms |

`/proc` reads of the loaded process took up to 2.28 s (`base2`), and 1.3 s
in forkbig (`baseh1`). On Linux they took at most 39 ms.

The box's signature shows here too: every canary, pinned or not, is late at
the same instant. A plain CPU load does not do it: in the runs whose quiet
phases are clean (`baseq1`, `proto3`), hog16's worst is 7-19 ms, as
Linux's. So neither wake placement nor EEVDF lag is the cause.

### 1.3 The mechanism

The instrumentation was local and unlanded (I1): per CPU, how long the
preemption count stayed raised as seen at interrupts; per lock site, waits
and holds over a threshold (`rdtsc`); and how late each timer interrupt came.

1. **Every processor spins in one process's locks with preemption off.** In
   each stall, all four CPUs had their count raised for 0.3-2.7 s, in
   fault8's case 190 s. They were waiting for:
   - the load process's `AddressSpace::inner` (`space.rs`: the fault's
     resolve at :999, `fill_file_page`'s lookup at :1193, user copies at
     :1455, `/proc` at :3859, `map_anywhere` at :2897, `give_back` at :1660);
   - a futex bucket (`futex.rs:750`);
   - a pipe (`pipe.rs`);
   - a VMO's lock (`vmo.rs:830`, `:884`, `:956`, `:1543`);
   - the machine-wide shootdown turn (`smp.rs:1028`).
2. **A waiter is not preemptible.** `PreemptSpinLock::lock` raises the count
   and then waits for its ticket. A processor whose count is raised makes no
   scheduling decision: the timer interrupt's exit returns at once
   (`preempt_on_irq_exit`).
3. **A timed sleeper is woken only by its own processor's decision**
   (`choose_next` → `wake_sleepers`). So while a CPU waits, its sleepers stay
   asleep, and no other CPU takes them. With a multi-threaded process
   contending, that is every CPU at once.
4. **Any delay of one holder becomes a machine-wide stall.** Ticket order is
   FIFO, so every waiter waits out every hold queued ahead of it. The holder
   is delayed by long holds, by a shootdown it waits for, by interrupts on
   its CPU, or, in a VM, by the host. In QEMU the holds measured were:

   | hold | site |
   |---|---|
   | 157-391 ms | fork's copy under the space lock (`space.rs:868`, 1 GiB) |
   | 155-379 ms | `Vmo::fork` under the VMO's lock (`vmo.rs:884`) |
   | 134-459 ms | `give_back`/`take_range` (`space.rs:1660`, `vmo.rs:1543`) |
   | 344-1,107 ms | the shootdown turn held while waiting for answers |

5. **A feedback.** In forkbig, the zombies of the forked children are reaped
   only by idle tasks, and no CPU ran its idle task. The VMO's mapper list
   (`vmo.rs:830`, `detach`'s `retain`) grew with them, and holds grew with
   the list: 128 s and 190 s of raised count on two CPUs.

Linux shows none of this, for three reasons:

- Its `mmap_lock` is a sleeping rwsem, and its waiters give their processor
  away.
- Faults run concurrently under per-VMA locks.
- Long operations (`copy_page_range`, zap) call `cond_resched`, and
  `CONFIG_PREEMPT` lets the holder be preempted.

This contradicts what MEMORY-AND-TIMING §2.2 claims: "Preemptible kernel, so
a long section delays rather than blocks". A long section blocks every
processor whose task waits for it. FM-8's "a job's share of a contended
processor is its weight's" also fails: the waiting is charged to the
spinning job, but nobody else gets the processor.

### 1.4 The prototype (local, I1)

The space lock's waiters wait preemptibly:

1. Raise the count and try the lock.
2. On failure, lower the count, and with it take any decision asked for
   meanwhile.
3. Watch the lock with the count at zero until it looks free, then go to 1.

Only the holder keeps preemption off. That is ~25 lines in `ferrix-sync`,
plus `space.rs`'s 28 call sites. Same loads, same host processors, at nice
0; `proto1` ran at a host load of 120 and is left out:

| phase | prototype (`proto3`, `proto4`, `protoh1`) | develop at nice 0 (above) | Linux 7.0 |
|---|---|---|---|
| quiet | 15-38 ms | 6-42 ms | 9-18 ms |
| hog16 | 7-33 ms | 19-334 ms | 10-48 ms |
| futex8 | 10-17 ms | 23-326 ms | 6-46 ms |
| pipe8 | 12-30 ms | 32-41 ms | 7-51 ms |
| fault8 | 11-57 ms | 305-316 ms | 22-56 ms |
| wine8 | 118-183 ms | 37-280 ms | 12-18 ms |
| mixed | 40-89 ms | 85-545 ms | 12-151 ms |
| bigunmap | 133 ms (p99 8-12) | 157 ms (p99 52-60) | 16 ms |
| forkbig | 582 ms | 190,656 ms | 21 ms |

With the prototype, `/proc` reads of the loaded process still took 233 to
1,278 ms: an unfair try-lock lets a reader that comes late lose to the
load's threads again and again, which is why the fairness bound (§3 A) is
owed in A's own landing.

What remains is the holds themselves: a 1 GiB fork or unmap still holds a
spin lock for 130-460 ms. Its own CPU runs nothing else meanwhile. Waiters
already inside another spin lock still wait with the count raised: the
fault's VMO lock is nested in the space lock. And the shootdown turn's holder
waits for answers with preemption off.

## 2. What the fix must give

- **R1, the waiting.** No task waits for a kernel lock with preemption off
  unless it already holds one. A processor whose task waits keeps making
  decisions, and its sleepers are woken on time.
- **R2, the holding.** No kernel spin lock is held, nor preemption kept off,
  for longer than a stated bound H, whatever a ring-3 program asks for. The
  bound is per operation, in pages per hold, not one hold over a range.
- **R3, the outcome.** Under a ring-3 load saturating every processor, a
  woken task of another job gets a processor within one slice plus H. That
  is the (I3) check's bound.

## 3. Options, with what each costs the kernel

Line counts are estimates of kernel lines added or changed, item ring
included, reasons per change.

**A. Waits that do not hold the processor (R1).** `PreemptSpinLock::lock`
takes the prototype's path when the caller's count is zero: the outermost
lock of a task.

- A nested acquisition keeps today's ticket, so lock-order reasoning and the
  nested holders' bounds stay as they are.
- Waiters hold no ticket, so a waiter switched out passes no stall on. That
  is the convoy `sync.rs` warns of: tickets taken by tasks that are then
  switched out.
- Fairness changes from FIFO to "whoever tries first after the release". A
  waiter that has watched longer than W (one slice) sets a handoff flag.
  While it is set, only that waiter may take the lock, which bounds its wait
  (Linux's mutex handoff).
- Cost: about 40-60 lines in `ferrix-sync` (the path, the handoff flag, the
  `#[track_caller]` site kept), no call sites changed, and a host test of the
  handoff.
- Every `crate::sync::SpinLock` changes behaviour. The consultant may prefer
  to scope it to `AddressSpace::inner`, the futex buckets and the pipe lock:
  the same ~40 lines plus about 35 call sites.

**B. The space lock becomes a sleeping lock.** Waiters sleep, and the holder
is preemptible, so a long hold stops only the threads of the process that
asked for it, as Linux's `mmap_lock` does.

- User copies already run where blocking is allowed: they fault a file page
  in from disk first (`uaccess.rs`). So their callers hold no spin lock.
- The reverse paths into the space lock are the problem:
  - `Vmo::retire` → `forget_runs`, reached from reclaim and truncation with
    VMO or list locks held;
  - the IRQ-masked checks;
  - the native fast path.
  Each needs a try-and-retry or a reordering.
- Cost: about 200-500 lines across `user/space.rs`, `user/vmo.rs`,
  `user/cache.rs`, `syscall/uaccess.rs` and `mm`, plus a lock-order
  restatement and the USER-COPY argument (V-01) restated over a sleeping
  lock.
- The VMO's own spin lock still needs C for its long holds.

**C. Linux's model: an rwsem plus per-region locks and lockless small
copies** (STALLS.md P4b). Faults run in parallel: the scalable end.

- Cost: 1,500-3,000 lines, new fault fixups, and the USER-COPY safety
  argument rewritten (V-01, SECURITY-TARGET O.VALIDATE, SPECULATION's
  `nospec_below`).
- It needs everything B needs.
- It is not needed for F-95. It is the scalability project after it.

**D. Long holds cut into bounded pieces (R2).**

- **Unmapping and giving back:** `take_down` and `give_back`, and
  `Vmo::take_range` over N pages, in batches of P pages. The lock is let go
  between batches. The `layout` sleeping lock still keeps the map from
  changing under an unmap, so a batch boundary is only a point where faults
  of other ranges may run.
- **Fork:** `Vmo::fork` copies the page map in batches under the VMO's lock.
  Pages committed between batches are copied by the next batch. The child
  is not visible until the copy ends, so nothing reads it half made. The
  space lock across fork stays one hold under A, and a preemptible one
  under B.
- **`detach`:** a dead mapper is cut in O(1). The `retain` over every mapper
  of a VMO goes, which also ends the zombie feedback.
- Cost: about 150-250 lines in `user/space.rs` and `user/vmo.rs`. Each
  operation's atomicity is restated.

**E. The shootdown turn.** `smp::SHOOTING` is a `crate::sync::SpinLock`,
taken by `take_turn`, which loops on `try_lock` and answers other
processors' shootdowns while it waits. Its callers:

| caller | reached from | count when it waits for the turn |
|---|---|---|
| `invalidate_pages` (`flush_tlb_pages`) | every unmap, protect, advise, fork's COW takedown, a VMO's retire, the reaper's stack frees | zero, except the reaper's |
| `flush_everywhere_within` (`flush_tlb_everywhere`) | kernel mappings changed (`mm::unmap_kernel_all`), the reaper's batch | zero, except the reaper's |
| `synchronize` / `synchronize_within` | grace periods (`smp::read_section` users, a speculation-domain leave) | zero |

`shootdown_requested` already refuses a shootdown with any spin lock held
(`locks_held == 0`, checked on every architecture), so no caller waits for
the turn inside another lock. The one hand raise is the reaper's
`preempt_disable` around freeing a batch of stacks (`sched::reap_batch`),
which runs in the idle task. So:

- A waiter for the turn already lowers its count after every failed
  `try_lock`, and with it makes any decision asked of it: it is preemptible
  today, and A does not change it. The reaper is the exception: it waits
  with its count raised by hand, and on an idle processor only.
- The holder waits for the answers with the turn held, so with preemption
  off. That wait is bounded by the slowest answer, which is the longest
  interrupt-masked span on any processor of the set or a host not running a
  virtual processor (FM-10's late bound). The 344-1,107 ms holds of the
  turn measured in §1.3 came with timer interrupts 159-514 ms late on the
  processor that answered last, and no `IrqSpinLock` wait over 5 ms
  (`proto2`): a host not running that virtual processor, not a masked
  span in the kernel, as far as I1 can tell. On hardware there is no host,
  and the bound is the masked span; R3 carries it as FM-10's residual.
- Not measured, and not ruled out: holds of the heap's lock (`HEAP`, taken
  with interrupts masked) and of a remote run queue's lock in a wake. If
  they contribute, `test-stall` shows it as a late canary while every
  space-lock and turn hold is short, and the I1 sampler as `irqwait`
  entries; `proto2` saw four over 5 ms, none over 50.

## 4. Recommendation

**A (general form) + D, now. B later, as the customer weighs slim against
scalable.**

- A plus D is about 200-300 kernel lines.
- A alone took every short-hold phase to Linux's range.
- D takes the long holds to a stated bound.
- B is about twice the lines again, and buys the property that a long hold
  stops only its own process. That matters for a 30-thread game process and
  for nothing else measured.
- C is a project of its own.

Order of landing:

1. **(I3) the stall check, reporting only.** Checks-only, on a code review.
2. **A,** with its requirement and controls. The check turns from reporting
   to asserting R3 for the short-hold phases.
3. **D,** per operation. The check asserts for bigunmap and forkbig too.

## 5. Requirements, checks, controls

**Requirements** (ids to be reserved in `requirement-reservations.json`
first):

- R1 and R2 as `L.sched.*` or `L.sync.*`, under `H.SCHED.1`.
- R3 under `H.SCHED.2` (latency).
- MEMORY-AND-TIMING §2.2 restated: "a long section delays rather than
  blocks" becomes true for waiters and is bounded by H for holders. §2.2a
  gains the wake-to-run bound. FM-8's and AoU-4's text is checked against it.

**The (I3) check** is `cargo xtask test-stall`, an xtask row. It boots
`src/tests/stall` (a Rust port of `stallrep`) as init: 4 vCPUs, KVM where
there is one, 5 s per phase. The phases are hog (the control), fault,
futex, bigunmap and forkbig. Each phase prints per canary the wake count,
p50, p99 and max. The row fails when any canary's max exceeds the bound for
its phase:

- 100 ms for the short-hold phases;
- 100 ms plus the stated H for bigunmap and forkbig.

The bound is set well above Linux's 10-56 ms in the same host noise, and
well below develop's 280 ms to 190 s.

**Negative controls,** with `gate.sh control --expect`:

| control | what it does | expected result |
|---|---|---|
| k1 | A's path disabled: `lock` always takes a ticket | the check fails on fault/futex |
| k2 | the handoff flag never set | a host test of starvation fails |
| k3 | D's batch size set to "everything" | bigunmap and forkbig fail |

**Gates:** `cargo xtask check`; test-boot on x86_64, aarch64, and armv7a at
`--smp 2`; test-stall; test-threads; test-compositor; bench-ipc ABAB
(`lock`'s fast path gains one load of the count).

**Coverage:** carried after the final rebase, with dropped anchors listed.

## 6. Questions for the reviewer

1. Is A acceptable in its general form, for every `crate::sync::SpinLock`
   when the caller holds none? Or should it be scoped to the measured
   locks?
2. Is the handoff flag the right fairness bound, or is FIFO among
   preemptible waiters required (a queue of waiters that are not
   spinning)?
3. Does D's batching of `take_range` and `Vmo::fork` need its own design
   review per operation, or can this one cover it with an atomicity
   statement each?
4. Where do R1-R3 live: one requirement each, under `H.SCHED.1` and
   `H.SCHED.2`?
5. May the (I3) check land reporting-only first, as I3 allows, with its
   bound written in the row and asserted from landing 2 on?

## 7. Where it stands

**Verdict, 2026-10-24: OK IF (1)-(24)** (consultant ledger line 3950). The
order is (1) the stall check, reporting only, (2) A, (3) D, and each
landing updates this section. What the verdict settled:

- §6's questions:
  - A may be general: every `crate::sync::SpinLock`, and every library lock
    through the preemption hooks, takes the preemptible wait when counting
    has started, the processor's whole count is zero and interrupts are on.
    Any other acquisition takes today's ticket.
  - A fairness bound is owed in A's own landing. The preferred form: a
    waiter that has watched for W takes a ticket with its count raised, so
    every wait is at most W + (N-1)·Hmax. A handoff flag is accepted only if
    its owner keeps its count raised from setting it until it holds the lock.
  - D is covered by this design, but each batched operation needs an
    atomicity addendum reviewed before its code. Making `detach` O(1) may go
    on a code review alone.
  - The requirements are new: H.SCHED.16 (wake-to-run against another job's
    saturating load) with two L.sched ids under it, and H.SCHED.17 (holds
    per operation, P pages) with one L.user id per operation. They are not
    under H.SCHED.2.
  - The check may land reporting only.
- The (I3) check must also cover Wine's pattern and `/proc` reads, and must
  carry a noise rule: a quiet phase before and after the load. A run whose
  quiet phase is late is void; it is run once more, and two voids fail.
- **Fork** under D: either the space lock is let go between `Vmo::fork`'s
  batches, with the parent write-protected first and its faults sleeping,
  or fork stays an exception with a stated per-page bound and forkbig stays
  reporting only.
- Option B, the sleeping space lock, is the customer's question, not this
  review's.

**Landing 1 (this one):**
- `cargo xtask test-stall` and `src/tests/stall`, every phase "reporting,
  not asserted".
- F-95 filed in FINDINGS.md.
- MEMORY-AND-TIMING §2.2 restated, and §2.2y started: the wait, and the
  census of holds a program can make long.
- FM-8 and AoU-4 pointed at F-95.

**The row's own figures on develop** (`~/.local/share/ferrix/sched-wake/`
`row-dev1.log` on c07af7a45, host load 33-42; `row-dev2.log` on 25be7e42d,
load 51-71, with the fault phase lengthened to 12 s for the try):

| phase | `row-dev1` | `row-dev2` |
|---|---|---|
| quiet / quiet2 | 7 / 3 ms | 15 / 2 ms |
| hog | 12 ms | 25 ms |
| futex | 31 ms, `/proc` 382 ms | 15 ms, `/proc` 283 ms |
| fault | 69 ms (4 s) | 50 ms (12 s) |
| wine | 39 ms | 45 ms |
| convoy | 55 ms | 24 ms |
| bigunmap | 125 ms | **286 ms** |
| forkbig | **6,441 ms**, `/proc` 6,485 ms | **1,431 ms**, `/proc` 1,638 ms |

So on the row's own shape develop fails only where a single hold is long:
forkbig every time, bigunmap at load 50-70, and the `/proc` read under
futex. The fault phase did not fail at 4 s or at 12 s in these two runs,
where stallrep's 15 s fault8 measured 305-316 ms at nice 0 (`diag3`,
`baseq1`). Its stalls there came with timer interrupts late on the
processor that answered last: the host, amplified by the waits. So the
phase stays at 4 s, and the evidence that A is what helps rests on
forkbig and `/proc` (190 s to 0.6 s, `baseh1` against `protoh1`), not on
fault. k1 will be read on those phases.

**Landing 2, A, as built:**
- `ferrix_sync::PreemptSpinLock::lock` waits preemptibly when
  `PreemptControl::may_wait_preemptibly` says so. The kernel's `Preempt`
  says so when three things hold: counting has started, `count_here() == 0`
  (locks and hand raises both), and interrupts are on (condition (5)).
- `try_lock`, both `try_lock_masked`s, `IrqSpinLock`, the run queues'
  plain locks and `lock_manually` are unchanged.
- Library locks (`HookedPreempt`) keep the ticket: the hooks are
  unchanged, and their locks (btrfs, the VFS) were not among the measured
  stalls. A later landing may carry the rule through `PreemptHooks`.
- **Why it adds no new kind of switch point.** The failed try's `enable`
  is the same `enable_from` → `decide_deferred` every guard's drop makes.
  With the count at zero and interrupts on, the task is preemptible at
  every instruction anyway (`preempt_on_irq_exit`). The `disable` that
  succeeds runs on the processor that later lets the lock go: nothing
  between it and the acquisition can switch.
- **Fairness, the preferred form of (6).** A waiter that has watched for
  `PATIENCE_NANOS` (one slice, `SLICE_NS`) takes a ticket with its count
  raised, and a try fails while any ticket is queued. So its wait is at
  most W + (N-1)·Hmax. A nested waiter's is (N-1)·Hmax, as before.
- **No second API.** The prototype's `lock_preemptible` and its 28
  call-site edits are not in it, and no I1 code is.
- **Requirements.** H.SCHED.16 (baselined: its check is the row), L.sched.93
  (the boot check `a_lock_waits_preemptibly_only_holding_nothing`) and
  L.sched.94 (ferrix-sync's host test). The reservation shrinks to
  H.SCHED.17, L.sched.95 and L.user.163-168.
- **test-stall asserts each load phase's wake.** The short bound is 100 ms;
  the long one is 1,000 ms for bigunmap and forkbig until D. The `/proc`
  reads stay reported: the futex phase's still take 0.1-1 s with A.
- **Measured locally with A** (`row-A1`, `row-ab-A-2`, `row-ab-A-3`; host
  load 52-125):

  | phase | with A | develop (`row-dev1`, `row-dev2`) |
  |---|---|---|
  | forkbig | 109-277 ms | 1,431-6,441 ms |
  | bigunmap | 58-183 ms | 125-286 ms |
  | short phases | 22-84 ms, one wine run 195 ms in a run whose quiet was 50 ms | 12-69 ms |

  `row-ab-A-1` and `row-ab-k1-1` stopped in stage 4's lock check ("no two
  processors' increments overlapped"), a host-starvation flake that fired
  with A and without it.
- **Controls.**
  - k1, the rule made to answer no: stops the boot in L.sched.93's check
    ("a task holding nothing, with interrupts on, may not wait
    preemptibly"), inside test-stall, in `row-ab-k1-2` and `-3`.
  - k2, patience that never runs out: ferrix-sync's
    `a_preemptible_waiter_is_served_within_its_patience_...` FAILED after
    3 s.
  - k-mask, the rule made to answer yes in every state: the same boot
    check's nested, by-hand and masked cases.
  - The gate.sh runs are cited in the commit.

**Next:**
- Then the atomicity addendum for D, and the reaper's feedback: zombies are
  reaped only by idle tasks, so under a saturating load they pile up and
  lengthen `detach`'s hold.

## 8. Addendum for landing 3: the holds cut, and the reaper (for review before code)

Condition (19) asks, for each operation batched, what another thread,
another process, reclaim or truncation can see at a batch boundary, and
what is checked again once the lock is taken back. Condition (20) asks
that a boundary be a point where the processor's count returns to zero:
every spin lock let go, the space's included. P is
`HOLD_PAGES = 512` (2 MiB of pages) per hold. The time per batch is
measured on each architecture by the rows and stated in MEMORY-AND-TIMING
§2.2y, with no WCET claimed (AoU-4). The requirement is H.SCHED.17, with
one L.user id each below.

### 8.1 `unmap`'s takedown (`AddressSpace::unmap`, phase one): L.user.163

**Today.** One hold of the space's lock takes the range out of the map
(`map.remove`), takes every translation in it out of the tables
(`take_down` → `mm::unmap_in`), and reads the shootdown's set.

**Batched.**
1. Under the `layout` sleeping lock, which `munmap`, `mmap`, `mprotect`,
   `mremap`, `brk` and shm all take first, one hold of the space's lock
   takes the range out of the map and notes what it removed. This step is
   O(regions), as today.
2. The removed range's translations then come down P pages a hold, with the
   space's lock let go between holds.
3. The last hold reads the shootdown's set, `begin_shootdown` as today.
4. The shootdown and `give_back` follow, as today.

**What another party sees at a boundary:**
- *Another thread of the process, faulting in the range:* no region, so
  `NotMapped`, the segmentation fault. That is what it sees after the
  unmap today, and what Linux gives after the VMA is gone.
- *Another thread touching a page still translated in a later batch:* the
  page, as it would have just before the unmap. Its frame is not given
  back until after the shootdown, so it is never another owner's.
- *Another thread mapping in the range:* it cannot. It needs `layout`, which
  the unmap holds throughout.
- *Another process:* none of this space's translations are its.
- **Reclaim or truncation of a file's page mapped in the range** (`Vmo::retire`
  → `forget_runs`): this is the case that rules out batching for
  file-backed regions. The region is already out of the map, so
  `forget_in` finds no mapping and does not take the translation down,
  and the frame could be given back while a later batch still translates
  it. **So only a region whose backing is anonymous and owned by this
  space alone is batched**: `Backing::Anonymous`, an object of strong
  count one in this space's table, decided in step 1. File and device
  regions keep today's single hold. An anonymous owned object is reached
  by no reclaim and no truncation, and a frame it shares after fork is
  held by its own object's reference until `give_back`.

**Checked again after each retake:** nothing of the map, since `layout` is
held. Each batch walks only the tables of its own pages, and a table
emptied by an earlier batch is already on the shootdown's list.

### 8.2 `give_back` and `Vmo::take_range`: L.user.164

**Today.** One hold of the space's lock decides which ranges' objects this
space holds alone (`strong_count == 1`) and takes all their pages out
(`take_range`, under the object's pages lock as well). The pages are
retired after the lock is let go.

**Batched.** For each freed range, P pages a hold:
1. Take the space's lock.
2. Decide again whether the object is still this space's alone.
3. Take that batch's pages out (`take_range(first + k·P, P)`).
4. Let go.
5. Retire them, which costs no shootdown: this space's translations are
   down, and no other space holds the object.

**What another party sees at a boundary:**
- *A fork between two batches:* it cannot happen, since fork takes `layout`
  (`process.rs` 778/849).
- *Another space taking a reference to the object between batches:* by
  `vmo_map` or a shm attach (shm takes `layout`; `vmo_map` of an object by
  handle does not). The per-batch decision then finds a strong count above
  one, and the rest of the range is left to the object, as today's "not
  this unmapper's to take".
- *A fault through another region naming the same object:* the range was
  removed from the map, so no region of this space names those pages.

**Checked again after each retake:** the strong count, and the object's
presence in the table (`owner`).

### 8.3 `Vmo::fork` and `AddressSpace::fork`: an exception, not batched

Condition (20) offers two ways for fork:
- let the space's lock go between batches, with the parent write-protected
  first and its faults sleeping;
- or state fork as an exception with a bound proportional to its pages.

This addendum takes the second. The first needs a fault that sleeps on the
space, which is option B's sleeping lock, the customer's question (C1).

So:
- fork keeps one hold of the space's lock and of each object's pages lock,
  and MEMORY-AND-TIMING §2.2y states its bound: proportional to the
  committed pages and regions of the parent (157-391 ms a GiB under KVM,
  measured on the box by the BACKLOG row).
- After landing 2, the hold stops only the forking task's own processor and
  the nested waiters on that object's lock. Any other waiter waits with its
  processor free.
- forkbig stays at the long bound in `test-stall`.
- F-95 cannot close on this addendum alone: (23) wants every phase at the
  short bound. The fork exception goes to the customer with B.

### 8.4 The reaper on a busy processor: L.sched.95

**What was seen** (`baseh1`, forkbig at 15 s on develop): 128 s and 190 s
of raised count on two processors.
- A dead task's stack, and the address space its last reference holds, are
  freed only by an idle task (`idle_loop` → `reap_batch`). No processor ran
  its idle task under the saturating load, so zombies, and the spaces they
  held, piled up.
- Every live space mapping the program's file stays in that file object's
  mapper list. So `Vmo::detach`'s `retain` over the list, and `attach`'s,
  grew with the zombies. Every fork and exit walked thousands of entries
  under the object's lock.

**The change.** A task being made pays for the dead first:
- `spawn_task` reaps up to `REAP_BATCH` zombies before it makes the new
  task, when the list holds more than `REAP_AHEAD` (4 × REAP_BATCH) and the
  caller may block (`may_block`: count zero, interrupts on).
- It uses the same `reap_up_to` the idle task does, preemptible, so the cost
  falls on the program that makes and ends tasks, not on the machine.
- The zombie list then stays under REAP_AHEAD plus one batch per processor
  making tasks at once, and the mapper lists stay as long as the live
  spaces.

**What it changes:**
- `fork`, `clone` and a kernel spawn sometimes free up to 16 stacks first,
  under one shootdown, as the idle task does.
- `spawn_task` is called from the check code and from `prepare_user` /
  `spawn_user`. A caller that holds a lock or has interrupts masked does not
  reap.

**Check:** a boot check makes 4 × REAP_AHEAD tasks that exit at once on a
processor kept busy, with no idle time on any processor (a spinner per
processor), and requires the zombie count never to exceed REAP_AHEAD + N ×
REAP_BATCH. Control: with the spawn's reap removed, the count grows past it.

`detach`'s `retain` is not changed. With the reaper fixed, its length is the
number of live spaces mapping the object, bounded by the processes
running. That is an O(n) hold with n the live mappers, as today. It goes
into §2.2y's census.

### 8.5 Requirements and controls for landing 3

H.SCHED.17: "A kernel spin-lock hold that a ring-3 request causes in the
operations listed is at most P pages of work: unmap's takedown of an
anonymous range it owns alone, and the giving back of its pages. Fork is
the stated exception." Under it:

| id | what | control (`gate.sh control`) |
|---|---|---|
| L.user.163 | takedown batched | `HOLD_PAGES = u64::MAX` (k3): bigunmap over its short bound |
| L.user.164 | give_back batched | the per-batch strong-count recheck removed: a boot check of an object mapped into a second space between batches FAILED |
| L.sched.95 | the reaper on a busy processor | the spawn's reap removed: the zombie-bound boot check FAILED |

Once landed, bigunmap moves to the short bound. forkbig stays at the long
bound, as the stated exception.
