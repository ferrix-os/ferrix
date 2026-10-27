# Real-time domains: what other kernels do, and how Ferrix can do it

Research for [stage 14](roadmap/stage-14-real-time-domains.md), 2026-10-26.
Nothing here is built. It surveys Linux (v6.12), seL4 MCS, QNX Neutrino,
Fuchsia's Zircon, and the partitioned RTOSes (ARINC 653: VxWorks 653,
INTEGRITY-178, PikeOS, Xen's a653sched), sets that against what Ferrix has
today, and proposes the shape stage 14 should take. Claims about other
systems were checked against their source where the sandbox could reach it
(Linux v6.12, seL4 master after 16.0.0, Xen, Zephyr); the rest come from
their documentation and are marked *(doc)*, or from memory, marked
*(unverified)*.

The design intent this answers is ARCHITECTURE.md §5: per-domain modes
(`Throughput`, `SoftRt`, `HardRt`), FIFO/RR, threaded interrupts, PI
mutexes, EDF with CBS admission that refuses an unschedulable set, and a
runtime mode switch through a quiescent point. The exit: a cyclictest-shaped
boot test on a `HardRt` domain while `Throughput` saturates the other cores,
and an admission test that refuses an unschedulable set.

## 1. What each system does

### 1.1 Linux (v6.12)

**Classes.** stop > deadline > rt > fair (EEVDF since 6.6) > ext > idle,
picked in that order. RT priorities 1-99, an O(1) bitmap of per-priority
lists per CPU (`kernel/sched/rt.c`); FIFO runs until it blocks or is
preempted, RR rotates within a priority every `sched_rr_timeslice_ms`
(100 ms, tick-granular). No global RT queue: push/pull migration keeps the
N highest-priority RT tasks running somewhere (`cpupri`, `rto_mask`, an IPI
chain for pulls).

**Protecting the fair class.** Until 6.11 RT was throttled to 950 ms of
every 1000 ms per CPU, which idled CPUs and gave RT 50 ms blackouts. 6.12
replaced it with a *deferred deadline server* (`fair_server`, 50 ms per
1000 ms): a SCHED_DEADLINE entity that competes only when fair tasks have
been starved to their zero-laxity point, and otherwise never fires.

**SCHED_DEADLINE.** EDF with a hard CBS: runtime Q, relative deadline D,
period P, Q ≤ D ≤ P, period 100 µs to 4.19 s. A task that exhausts Q is
throttled until a replenishment hrtimer; a wakeup that would overflow its
bandwidth gets a fresh deadline. Admission is per *root domain*:
Σ Q/P ≤ 0.95 × the domain's capacity, refused `EBUSY`. For global EDF that
bounds tardiness only (Dhall's effect); hard guarantees need a one-CPU root
domain, which you get only through exclusive cpusets. A task may become
SCHED_DEADLINE only if its affinity covers its whole root domain, a known
usability trap. GRUB (`SCHED_FLAG_RECLAIM`) lets reservations use idle
bandwidth. Forking a deadline task is `EAGAIN` unless reset-on-fork: the
child was never admitted.

**Priority inheritance.** `rt_mutex`: an owner word, an rbtree of waiters
by priority then deadline, a per-task tree of top waiters, and an iterative
chain walk bounded by `max_lock_depth` (1024) with deadlock detection.
PI futexes (`FUTEX_LOCK_PI`, `UNLOCK_PI`, `TRYLOCK_PI`, `LOCK_PI2`,
`CMP_REQUEUE_PI`) put an rt_mutex behind a TID word; robust lists release a
dead owner's locks. A deadline waiter boosts its owner with its *deadline*
but not its *bandwidth*: the owner runs on the donor's parameters with
immediate replenishment, which breaks isolation (Linux's own documentation
lists it as future work; proxy execution is the long-term fix).

**PREEMPT_RT** (selectable in mainline from 6.12, x86-64, arm64, riscv;
not 32-bit ARM). `spinlock_t` and `rwlock_t` become PI sleeping locks;
`raw_spinlock_t` is the only truly spinning lock and must be short and
bounded; `local_lock_t` names per-CPU sections; every device interrupt is
forced into a SCHED_FIFO thread at priority 50 (timers and IPIs excepted);
softirqs run in the raising task's context. It took twenty years mostly
because "atomic context" was everywhere and synchronous console printing
was the last blocker.

**Isolation.** `isolcpus` (boot, irreversible), `nohz_full`, `rcu_nocbs`,
IRQ affinity, and cgroup v2 cpuset partitions (`root`, `isolated` since
6.1, `cpus.exclusive` 6.7, workqueues excluded 6.8). A partition change at
runtime migrates tasks, rebuilds scheduler and root domains, and re-runs
deadline admission (and can fail if bandwidth does not fit); the tick, RCU
callbacks and managed interrupts remain boot-time.

**Measurement.** cyclictest: each thread sleeps to an absolute
`CLOCK_MONOTONIC` time and records how late it woke, under load, as a
histogram; report the worst case. hwlat (IRQs-off spin to find hardware and
SMI noise), osnoise and timerlat (per-source attribution). Typical worst
cases under load: ~10-50 µs tuned x86 PREEMPT_RT on an isolated core,
hundreds of µs to ms on a non-RT kernel *(unverified, order of magnitude)*.

### 1.2 seL4 MCS

**Time is a capability.** A *scheduling context* (SC) holds a budget, a
period and a bounded ring of refills (a sporadic server after Stanovich et
al., RTAS 2010). It is configured only through a per-core `SchedControl`
capability; budget == period means round-robin. The minimum budget is twice
the kernel's WCET, so a thread can always enter and leave the kernel. SCs
are per core: MCS on SMP is partitioned.

**Donation.** A passive server has no SC. `seL4_Call` donates the caller's
SC with a reply object, so the server runs on the client's priority *and
budget* (bandwidth inheritance, not only priority). Reply returns it. A
timeout endpoint receives a fault carrying a badge when a donated budget
runs out, so a supervisor can reset the server.

**Priorities.** 256 fixed priorities; a thread may set another's priority
only up to its own *maximum controlled priority*: priority as a delegable,
non-escalating authority.

**Admission is not the kernel's.** The kernel enforces budgets and never
checks feasibility; EDF and mixed-criticality schedulers are user-level
(Lyons et al., EuroSys 2018 *(unverified bibliographic details)*).

**Kernel.** Not preemptible; long operations call `preemptionPoint()` and
restart from saved progress. Its WCET analysis (Blackham 2011, Sewell 2017)
is archived because Arm no longer publishes instruction latencies for
out-of-order cores.

**Interrupts** reach a user thread through a notification: threaded by
construction, their cost bounded by that thread's SC.

**The domain scheduler.** A static cyclic schedule of (domain, duration)
entries above the priorities; only the current domain's threads run. Since
15.0.0 (RFC-20) schedules are configured at runtime
(`seL4_DomainSet_ScheduleConfigure`) and switched atomically
(`ScheduleSetStart`), the recommended use being to build the new schedule
past the end marker and then switch: "wheels up / wheels down" modes,
ARINC 653. Editing the running entry takes effect only when it is next
reached. Domains are uniprocessor only. The kernel does no draining.

### 1.3 QNX Neutrino

Priority-preemptive FIFO, RR and sporadic scheduling *(doc)*; a sporadic
thread that exhausts its budget drops to a low priority rather than
blocking. **Priority inheritance through messages**: `MsgReceive` gives the
server thread the sender's priority, but it is not reverted on `MsgReply`, a
known pitfall; with the adaptive partitioning scheduler the server's time is
also billed to the client's partition. **APS** gave partitions percentage
budgets over a 100 ms window, lent idle time to others, let critical
threads run on a separate budget and declared a partition "bankrupt" when
that ran out; QNX 8.0 no longer supports it. Interrupts moved toward
threads: 8.0 recommends `InterruptAttachThread`, a thread bound to one IRQ,
as the lowest-latency path.

### 1.4 Fuchsia / Zircon

Per-CPU fair and deadline run queues, picked deadline first *(doc)*. A
*profile* object carries `{capacity, relative_deadline, period}`; there is
no admission control, and overload stretches every period. A privileged
`RoleManager` component maps role names to device-tuned profiles: policy in
user space, enforcement in the kernel. **OwnedWaitQueue** propagates the
waiters' scheduling parameters to a wait queue's single owner and down the
chain, exposed through futexes that name their owner. A deadline thread is
given the highest priority as a "temporary workaround" for PI, which shows
how awkward mixing EDF and priorities in one inheritance chain is. Whether
channel calls donate is unverified; this research found nothing saying
they do. **Interrupts** are objects that wake a waiting thread or queue a
packet on a port, ahead of other packets: Ferrix's own shape.

### 1.5 ARINC 653 and the partitioned RTOSes

A fixed major frame of partition windows; priority scheduling inside each.
Part 2's multiple module schedules let an authorised partition request a
new schedule, which takes effect at the end of the current major frame
(INTEGRITY-178, XtratuM, PikeOS) *(doc)*. PikeOS adds a background
partition that runs in any slack. Xen's a653sched validates that the
windows fit the frame and switches immediately, mid-frame. Zephyr's EDF is
only a tie-break within one priority, with no budgets; its meta-IRQ
priorities are interrupt bottom halves above everything.

### 1.6 Mode changes in the literature

Real & Crespo (Real-Time Systems 26(2), 2004) classify mode-change
protocols as *synchronous* (new-mode tasks start once the old ones are done
or aborted, at a quiescent point; no transition analysis) or *asynchronous*
(both run during the transition; needs offline analysis, e.g. their Minimum
Single Offset). Products all follow one pattern: precompute and validate the
new configuration, then switch in one authority-checked step at a defined
boundary, with draining left to the application. Vestal's mixed-criticality
model (RTSS 2007) drops low-criticality work when high-criticality work
overruns its low estimate; seL4's timeout faults are the OS-level detector
for it.

### 1.7 Side by side

| | Linux | seL4 MCS | QNX | Zircon | ARINC 653 |
|---|---|---|---|---|---|
| Real-time policy | EDF+CBS > FIFO/RR > fair | 256 FP + per-core budgets; static domain cycle | 256 FP, FIFO/RR/sporadic | per-CPU EDF > fair | cyclic windows, FP inside |
| Time as an object | thread attributes | scheduling-context capability | thread attributes | profile object | partition window |
| Admission | Σ U ≤ 0.95·M per root domain | none (user level) | none | none | offline |
| Inheritance over IPC | n/a; PI futex | budget and priority donated on Call | priority (and partition) on receive | futex owner; channels unverified | n/a |
| Interrupts | hard IRQ + threads (RT) | notification to a thread | ISR or IRQ thread | object or port packet | vendor |
| Kernel preemption | fully (RT) | none; preemption points | mostly | preemptible | varies |
| SMP for RT | global or partitioned by cpuset | partitioned | global + affinity | per-CPU | per-core schedules |
| Mode switch | cpuset partition change | `ScheduleSetStart`, immediate | APS reconfiguration | role change | at major-frame end |

## 2. What Ferrix has today

From a survey of the tree on 2026-10-26.

**Built and usable:**

* Per-CPU EEVDF run queues (`ferrix-sched`, host-tested), tickless: one
  per-CPU one-shot armed for the earliest of slice end, first sleeper and
  `cpu.max` runout (`sched/timer.rs`, `queue.rs` `arm_timer`).
* A preemptible kernel outside spin locks: preemption at interrupt exit and
  when the preemption count returns to zero (`sched/preempt.rs`).
* The domain abstraction (`src/lib/kernel/sched/src/domain.rs`): `Mode`
  with `SoftRt` and `HardRt` *refused* (`ModeUnavailable`), `Class` with
  only `Fair` and `Idle`, `steals_work()` false outside `Throughput`, a
  256-CPU `CpuSet`, and `check_partition` requiring every CPU in exactly one
  domain.
* A grace period, `smp::synchronize`, documented as what the mode switch
  will wait on.
* Interrupts already threaded in effect: `object/interrupt.rs` masks the
  line, queues a packet on the bound port and wakes the driver thread; a
  storm bound masks an unacknowledged MSI after 64 deliveries.
* `cpu.max` throttling (a cap enforced at return to user mode).
* The microkernel plan: K7 tickets bind a server thread to a client's job
  for the length of a request ("the shape of seL4 MCS's scheduling-context
  donation"); K9 is to charge that thread's time to the client's budget.
* GAME-PATH §5 designs a `SoftRt` "game domain" taken out of `Throughput`
  at run time and dissolved when its handle closes; `tools/common/game/rig.c`
  has a cyclictest-shaped wake mode.

**Missing or in the way:**

1. No class dispatch: `CpuQueue` has only `fair`, and `choose_next` calls
   its `pick_next` directly.
2. One domain, a `Once` set at boot, read lock-free by the wake and steal
   paths.
3. The periodic `balance()` ignores `steals_work()` (the wake and steal
   paths honour it).
4. `sched_setaffinity` is validated and not applied; no cpuset controller;
   no interrupt steering (MSI targets the CPU that asked, GICv2 the enabling
   core).
5. No owner-aware locks: `SleepLock` has no owner field, futexes are FIFO,
   and the PI operations answer `ENOSYS`.
6. Spin-lock waiters spin with preemption off, and load-ring holds were
   measured from 128 ms to 128 s (PREEMPTIBLE-WAITS.md, F-95). Option A,
   preemptible outermost waits, is approved and not landed. No hard
   real-time claim is honest until it is.
7. Interrupt time is charged to the task it interrupted ("the victim
   pays", MEMORY-AND-TIMING.md §2.2).
8. `IDLE` and `PULLED` are 64-CPU masks where `CpuSet` allows 256.
9. `sched_setscheduler` refuses everything but `SCHED_OTHER`;
   `sched_setattr` is not in the syscall table; `RLIMIT_RTPRIO` is stored
   and not enforced.
10. No requirement covers real-time classes, EDF, admission, PI or mode
    switching; the safety manual marks ASR-8 ("admission of a real-time
    workload shall be refused when the set is unschedulable") partially met
    pending this stage. The scheduler is in the core ring.

## 3. The design this suggests

### 3.1 Domains are the partition, and own their admission

Linux attaches deadline admission to root domains reachable only through
exclusive cpusets, and refuses a deadline task whose affinity does not
cover its whole domain. Ferrix already has the better object: a `Domain` is
a disjoint CPU set with a mode. Make it the unit of admission, of interrupt
placement and of the mode switch, created and dissolved through a handle
(GAME-PATH's session-held handle), with the right to create one an explicit
capability.

### 3.2 HardRt: partitioned EDF with a hard CBS, admitted in the kernel

Every system that gives hard guarantees on SMP partitions (seL4's per-core
SCs, ARINC per-core schedules); global EDF admission is only a tardiness
bound, and Linux's is unsound with affinities (RTNS 2021). So, as
ARCHITECTURE.md already says:

* One EDF queue per `HardRt` CPU, no stealing, in a preallocated,
  fixed-capacity structure (no allocation on the RT path).
* A *reservation* object `{budget, period, deadline, cpu}` held by handle,
  minted only through a per-domain control right (seL4's `SchedControl`,
  Zircon's profile resource). Hard CBS: a reservation that exhausts its
  budget is throttled to its replenishment, never allowed to postpone its
  deadline and run on (soft CBS breaks the isolation argument). seL4's
  bounded refill ring is a ready, published algorithm for it.
* Admission when a reservation is placed on a CPU: Σ budget/period ≤ U_max
  for implicit deadlines (exact for one-CPU EDF), a density test when
  deadline < period, refused with an error the caller sees (the exit's
  "admission test that refuses an unschedulable set"). O(1) per change, so
  it is an analysable invariant. Unlike seL4 and Zircon, the kernel says no;
  *which* task gets bandwidth stays a user-level decision (a resource
  manager, as Fuchsia's RoleManager).
* Budget accounting on the existing one-shot timer, armed for the earlier
  of budget exhaustion and the next deadline event: exact enforcement, not
  Linux's tick-granular overrun.
* A minimum budget derived from the measured longest kernel path, as seL4
  derives it from its WCET bound.
* `HardRt` is "EDF and nothing else". A best-effort background class may run
  only in EDF idle time (PikeOS's background partition), never interleaved
  with deadlines.

### 3.3 SoftRt: fixed-priority FIFO/RR above EEVDF, with a server for the fair class

* An O(1) bitmap of 99 priority lists per CPU, Linux's numbering, for
  `SCHED_FIFO` and `SCHED_RR` from the Linux personality; RR's quantum on
  the one-shot timer rather than a tick.
* Protect the fair class the 6.12 way, not the old way: a deferred server
  that reserves a share for fair tasks and competes only when they have
  been starved, instead of 50 ms blackouts. GAME-PATH's floor (never fewer
  than two `Throughput` CPUs) is the machine-level form of the same
  protection.
* `RLIMIT_RTPRIO` and `CAP_SYS_NICE` enforced as Linux does; seL4's
  maximum controlled priority is the native form: a handle's rights bound
  the priority it can confer.

**As built (step 4b, `desktop`, 2026-10-27; SCHED-CLASSES.md §9e, ledger
line 29).** `ferrix-sched-fifo` is the tier: a balanced tree of the tasks'
own nodes keyed by priority and arrival rather than a bitmap of lists (the
review accepted the class's own tree). Its constants are fixed in the crate,
never run-time settings (R13): the round-robin quantum is **100 ms**
(Linux's default `sched_rr_timeslice_ms`), restarted after a yield and after
a preemption, and the floor is **950 ms of every 1 s period** per processor
(Linux's default `sched_rt_runtime_us`): while fair tasks are runnable
there, real-time tasks run at most that, so fair tasks keep at least 50 ms;
with none runnable the throttle does not bind. The floor is the throttle
inside the class, not the deferred server above: a server needs an EDF tier
`desktop` lacks (the review's answer 2). `sched_rr_get_interval` answers
the quantum.

**The guarantee is narrow.** Real-time tasks run in `desktop`'s boot
domain, `Throughput`, beside fair ones (the review's answer 3): what holds
is priority order, preemption on a wake (L.sched.109) and the floor
(H.SCHED.19, H.SCHED.20), and nothing more: no isolation of processors, no
bound on the kernel's non-preemptible sections (F-95), no threaded
interrupts, and no priority inheritance through user locks
(`FUTEX_LOCK_PI` stays `ENOSYS`). A woken real-time task stays on its own
processor (L.sched.110): a higher priority waits for that processor even
while another is idle. A `SoftRt` domain is compiled in and refused by name,
"compiled, not yet creatable", until those land (L.sched.115).

**C2.** A `SCHED_FIFO` task at priority 99 starves the kernel's own fair
worker tasks -- the reaper, the network's -- on its processor for 950 ms of
every second; the floor is what bounds that, and nothing else does.
`/proc/loadavg` counts real-time tasks as tasks of ordinary weight.

### 3.4 Inheritance: one owned wait queue, and donation over channels

* An owned wait queue with a single owner, waiters ordered by priority then
  deadline, and an iterative chain walk with a static depth bound (Linux's
  rt_mutex walk; Zircon's OwnedWaitQueue). It serves `FUTEX_LOCK_PI` and
  friends for the Linux personality (with robust-list release of a dead
  owner) and a PI `SleepLock` for kernel and native use.
* For channel calls, donate the caller's reservation, not only its
  priority: seL4's passive servers. `channel_write_read` to a waiting server
  is exactly a `seL4_Call`; K7's ticket already binds the server thread to
  the client for the request, and K9 is to charge its time there. Return the
  reservation atomically at reply (QNX's not reverting is the pitfall), and
  send a budget-exhausted packet to a supervisor port when a server runs out
  of a client's budget (seL4's timeout fault).
* Avoid Linux's gap: a deadline waiter must not give its owner unbounded
  runtime. Inherit the reservation (bandwidth inheritance) or, inside a
  `HardRt` domain, keep resource sharing to protocols with a single-blocking
  bound (SRP/priority ceiling) for in-kernel locks.

### 3.5 Interrupts: give the threads a budget and a home

Ferrix's top half is already seL4's and Zircon's: mask, queue, wake.
Stage 14 adds:

* Placement: a per-line destination so a domain's devices interrupt only
  its CPUs (MSI destination, I/O APIC, GICv3 `IROUTER`; GICv2 by enabling
  core), the open G3 question of GAME-PATH.
* Accounting: the top half's time charged to the line's holder or bounded
  per domain, not to the victim; the driver thread of a `HardRt` device
  holds a reservation like any task.
* Ordering: interrupt packets ahead of others on a port, or a dedicated port
  per real-time driver; QNX's lesson is that a thread bound to exactly one
  line is the lowest-latency threaded design.

### 3.6 Bounded kernel sections before "fully preemptible"

PREEMPT_RT's route (sleeping PI spinlocks everywhere) is twenty years of
work and multiplies what an analysis must cover. seL4's route fits a
certified core better: short spin sections with a measured bound, explicit
preemption points in long operations (revocation, VMO and address-space
teardown) that restart from saved progress, and a published number for the
longest non-preemptible path. The prerequisite in Ferrix is F-95's option
A, preemptible outermost waits, and the long-hold census; then the lock
types say by type which sections are bounded (Linux's `raw_spinlock_t`
versus `spinlock_t` lesson), and a check fails a section that is not.
Console output must never be synchronous on a real-time CPU.

### 3.7 The mode switch: prepared, then one step, at a quiescent point

Every product precomputes and validates, then commits atomically; none
drains in the kernel. ARCHITECTURE.md's protocol (stop admitting, grace
period, migrate, swap the class stack, resume) is a *synchronous* protocol
in Real and Crespo's terms, the simplest to certify. Concretely:

1. **Prepare** while the old mode runs: the target class stack, and every
   reservation the new mode will hold admitted against the target CPU set.
   If it does not fit, refuse before anything stops (Linux's cpuset change
   fails the same way; ARCHITECTURE.md's "refusal, not a fallback").
2. **Stop admitting** into the domain.
3. **Quiesce**: each CPU of the domain passes a scheduling point (an IPI
   rendezvous), then `smp::synchronize` lets lock-free readers of the old
   domain finish. The domain is a `Once` today; it becomes an RCU-published
   pointer.
4. **Re-home housekeeping**: move tasks the new mode cannot represent (each
   demoted with an error the switching caller sees), steer the domain's
   interrupts, move timers and deferred work off the CPUs. Linux leaves the
   tick, RCU callbacks and managed interrupts boot-time; Ferrix should move
   all of them here.
5. **Swap** the class stack in one store, **resume**.

Document what holds during the transition: no deadline is guaranteed until
the swap, and the transition's length is bounded by the longest
non-preemptible section plus the IPI latency plus the migration work.
Never edit a running configuration in place (seL4 defers edits of the
running schedule entry for this reason). An ARINC-style static time
partition schedule *between* domains sharing CPUs is MICROKERNEL.md's
maximum-certification profile (`sched_windows=arinc`), a separate later
feature. Inside each window it schedules by fixed-priority preemption, as
ARINC 653 and the certified partitioning RTOSes do (the customer,
2026-10-27; SCHED-CLASSES.md C6), not EDF.

### 3.8 Measure as Linux does, and say what is not isolated

* The exit test is cyclictest's shape: threads in the `HardRt` domain sleep
  to absolute deadlines and record their lateness in a histogram while the
  `Throughput` CPUs are saturated; the worst case is the result, measured
  for long enough to mean it. `rig.c`'s wake mode is a starting point.
* An hwlat-style IRQs-off spin to separate hardware, SMI and virtualisation
  noise from the kernel's; TCG numbers are not latencies, so the gate needs
  KVM and the box.
* Disjoint CPU sets do not isolate the shared last-level cache or memory
  bandwidth: the exit test's saturated `Throughput` cores will show it.
  That interference is unanalysed unless cache partitioning or bandwidth
  regulation (Intel RDT/CAT/MBA, Arm MPAM) is added; say so in the claim.
* As seL4 found, a sound static WCET on out-of-order cores is out of reach
  from vendor data: the evidence is measurement plus static structural
  bounds (loop bounds, chain depths, the longest section), which matches
  ARCHITECTURE.md's "does not promise a certified worst-case execution time".

How the classes are packaged -- each a removable crate behind one
compile-time trait, chosen per profile -- is [SCHED-CLASSES.md](SCHED-CLASSES.md).

## 4. A possible order of landings

The packaging of the classes, reviewed OK IF S1-S14 on 2026-10-26, and
where it stands: [SCHED-CLASSES.md](SCHED-CLASSES.md).

Each needs the consultant's design review and its requirement ids reserved
on `develop` first (GAME-PATH §5.6).

1. **F-95 option A** and the long-hold census: preemptible outermost waits.
   Prerequisite for any latency claim.
2. **Class dispatch and a mutable domain**: a class stack per `CpuQueue`,
   the domain an RCU-published pointer, `balance()` honouring
   `steals_work()`, the 64-CPU masks widened, `sched_setaffinity` applied.
3. **SoftRt**: FIFO/RR, the fair server, `sched_setscheduler` and
   `sched_setattr`, `RLIMIT_RTPRIO`; domain creation and the switch protocol
   for `Throughput` ↔ `SoftRt` (GAME-PATH's G1 to G5 are this slice).
4. **Owned wait queues**: PI futexes, robust release, a PI `SleepLock`.
5. **Interrupt placement and accounting** per domain.
6. **HardRt**: reservations, hard CBS, per-CPU EDF, admission, the
   `SCHED_DEADLINE` mapping (with fork refused unless reset-on-fork).
7. **Donation over channels** (K9): the reservation travels with the call,
   with a budget-exhausted packet.
8. **The exit**: the cyclictest-shaped boot test and the admission test,
   gated under KVM, with the measured bound written into the claim.

## 5. Sources

* Linux v6.12: `kernel/sched/{rt,deadline,cpupri,syscalls,core,fair,isolation}.c`,
  `kernel/locking/{rtmutex,spinlock_rt}.c`, `kernel/futex/pi.c`,
  `kernel/irq/manage.c`, `kernel/softirq.c`, `kernel/cgroup/cpuset.c`;
  `Documentation/scheduler/sched-deadline.rst`,
  `Documentation/locking/{rt-mutex-design,locktypes,pi-futex}.rst`,
  `Documentation/trace/{osnoise,timerlat}-tracer.rst`; LWN 934415 (deadline
  servers).
* seL4: `include/kernel/sporadic.h`, `src/object/{schedcontrol,schedcontext,domain}.c`,
  `src/model/preemption.c`, `CHANGES.md` 15.0.0 and 16.0.0, RFC-20
  (domain schedules); Sewell, Kam and Heiser, Real-Time Systems 53(5),
  2017; Lyons et al., EuroSys 2018.
* QNX 7.1 and 8.0 system architecture and adaptive partitioning guides;
  "Dissecting the QNX Adaptive Partitioning Scheduler", RTAS 2021.
* Fuchsia: kernel scheduling and fair scheduler concepts,
  `zx_profile_create`, futex and interrupt references,
  `fuchsia.scheduler.RoleManager`.
* Xen `xen/common/sched/arinc653.c`; Zephyr `kernel/Kconfig`; ARINC 653
  Part 2 as INTEGRITY-178 and XtratuM describe it.
* Real and Crespo, Real-Time Systems 26(2), 2004; Vestal, RTSS 2007;
  Stanovich et al., RTAS 2010; "On the Defectiveness of SCHED_DEADLINE
  w.r.t. Tardiness and Affinities", RTNS 2021.
