# Wake-affine and creation placement

Design, 2026-10-22, stream `wake-affine`. It is an addendum to the EEVDF
lag design note (ef-sys, 2026-10-21; consultant ledger 2742), and it builds
on the idle wake placement L.sched.85 (ledger 2658, 2984, 3052). Core ring:
`src/kernel/src/sched/**` and `ferrix-sched`. The design review comes
before any code.

## 1. What was measured

Agent Y's prototype is `eevdf-coalesce` dca552289, behind
`ferrix.coalesce=affine`. It was measured with `efsched` under KVM with 4
vCPUs, beside Linux 7.0 in the same guest shape. Two placement patterns
separate Ferrix from Linux. Under Wine both are everywhere: the
wineserver/client futex and socket hand-overs, and the render and present
threads.

- **(a) Wakes stay at home.** Linux runs a futex wakee on its partner's
  processor 7,608 times out of 7,609 (`wake_affine`). Ferrix wakes it at
  home every time (7,820 of 7,820), so each hand-over preempts a second,
  busy processor.
- **(b) New threads stay with their creator.** Ferrix leaves a new thread
  on its creator's processor when no processor is idle (18,064 of 24,066
  light wakes on one processor). Linux spreads new tasks at fork and exec.

**Why (b) happens, read from the code.** `choose_cpu` ranks a busy
processor on the count of runnable tasks, then on the load average.
Pollers sleep as soon as they are created, so the count is the same
everywhere: one spinner each. The averages are still ramping towards the
spinners' level (33 ms half-life), so every poller in the burst goes to
the processor whose average is lowest. That is the same processor each
time, because a sleeping poller adds nothing to it. In this branch's
fixed-phase runs, the 12 pollers sat on one or two processors in all 8
boots (all 12 on one in 3). The busiest spinner saw 4,100 to 7,000
involuntary switches a second, against about 1,000 if the pollers were
spread out. llvmpipe's
rasterisers finish a frame together, so the slowest spinner sets the
frame time.

Linux avoids this because a new task's PELT load starts at its full
weight (`init_entity_runnable_average`), and it is attached to the queue
the task is placed on. `sched_balance_find_dst_group` then sees that
load, and the burst spreads. The load decays with the same 32 ms
half-life as everything else.

**The prototype's p99 outliers (802 and 1,974 us).** The prototype moved
only 6 to 19 wakes in a whole run (its `affine-moved` counter). That is
enough to put the pong pair on one processor for good, but each move is
too rare to be a 1% tail by itself. Both outliers come from runs of the
random-phase harness (`efsched4`, r1 and e2). In those runs the pollers'
phases collided: six pollers within 140 us of each other in r1 and four
within 40 us in e2, all on the processor that also held the pair. On the
fixed-phase harness (`efsched6`, which is `efsched5` with a light-wake
lateness breakdown per processor), four rotated rounds of prototype
against main gave this:

| light-wake p99, us | noisy | heavy |
|---|---|---|
| main (prototype off) | 230, 223, 231, 230 | 366, 405, 446, 231 |
| prototype affine | 223, 230, 223, 223 | 223, 437, 223, 223 |

So the outliers are a placement-density effect (too many pollers, plus the
pair, on one processor) made worse by colliding phases. They are not a
cost of the affine move. The mechanism behind the tail is a woken poller
that carries negative lag. A poller that ran while its neighbours waited
comes back owing (`ferrix-sched` returns negative lag, where Linux's
DELAY_ZERO clips it to zero; note §6.5). It is ineligible until the
spinner has paid the debt off, which takes up to two slices. In
`efsched6`, the light wakes later than 300 us were mostly on the
processor that held the pong pair, with the pair running there in the
millisecond before (72 of 74, 117 of 140). Late wakes also came where the
pair was elsewhere (43 of 43 in one boot), so the pair is a large
contributor and not the only one. Rule C spreads
the pollers, which lowers that density. Rule A keeps its own move inside
the bounds in §2.

## 2. Rule A: a wakee goes to its waker's processor (wake-affine)

`wake(task)` (Wake::Home) from a task in a system call:

1. **Who may be pulled.** All of these must hold, or the wake is the wake
   at home exactly as on main, with L.sched.85's idle placement after it:
   - The waker is a task inside a system call on `here` (`IN_CALL`), and
     interrupts are enabled. An interrupt handler is never a waker.
   - The domain steals work (`Mode::steals_work`, the throughput domain),
     and it holds `here`. A soft or hard real-time domain keeps a task
     where it was put.
   - The wakee is not pinned, and its affinity allows `here`
     (`may_place`).
   - The wakee's home is not `here`.
   - Rule B does not refuse the wake.
2. **Idle comes first** (Linux's `wake_affine_idle`, then
   `select_idle_sibling`). If the home processor runs its idle task, the
   wake is the wake at home. Waking there costs no preemption and keeps
   the cache. Otherwise, if any processor that L.sched.85 could take is
   idle (in `IDLE`, allowed, in the domain), the wake is also the wake at
   home, and L.sched.85 places the task on that idle processor. The pull
   is tried only when no such processor is idle. `here` is never idle,
   because the waker runs on it.
3. **Weight** (Linux's `wake_affine_weight` with WA_BIAS, imbalance_pct
   117). The wakee is pulled only if `here`'s load average × 100 is below
   home's × 108. Ferrix has no per-task load average, so the wakee's own
   load is left out of both sides. That is generous to the pull for a
   heavy wakee. A task woken from a call is the hand-over case, which is
   light, and §5 measures the heavy case.
4. **The move** is `wake_onto`'s, on main since os-35's ipc-wake and used
   by Wake::Sync. It moves only state 2 (asleep at home), under both
   queues' locks taken lower processor first, and re-checks the task's
   processor after locking. The difference from Sync is the condition in
   (3), which replaces Sync's "nothing waits on `here`". The other
   difference is the decision afterwards: a Home wake does not promise
   that the waker blocks. So the decision is `kick_after_wake`'s
   undeferred one, the same as a local wake at home: `resched_here` if the
   wakee should preempt.

**The withdrawn re-placement hazard, restated.** `wake()`'s comment on
main says that re-placing a task at wake-up was tried twice and withdrawn
both times. A waker that reasoned about one of the "blocked" states moved
or unfiled a task that something else still believed it owned. `wake_with`
has since named the three states (part-way into `block`, asleep at home,
filed elsewhere). `wake_onto` moves only the second, decided under both
locks. Rule A adds no new move. It calls the same function under a
different load condition. The comment in `wake()` is rewritten to say so,
and to say which states are never moved.

**Not changed.** Wake::Sync (channels) keeps its own rule, and
`hand_over` (the direct switch) is not touched. The fast path never
reaches `wake()`. Rule B's bookkeeping is done only on `wake()`.

## 3. Rule B: wake_wide, so that a 1:N waker never pulls

Each task keeps three words, written only by the task itself while it
runs: the id of the last task it woke, a flip count, and the time the
count was last halved. On every wake that passes (1)'s first four
conditions, the waker records the wakee (Linux's `record_wakee`):

- If more than a second has passed since the last halving, the flip count
  is halved first.
- A different wakee from last time adds one flip.

The wake is wide, and the pull is refused, when
`min(flips) >= factor && max(flips) >= min(flips) × factor`. Here `factor`
is the number of processors in the domain, which stands in for Linux's
`sd_llc_size`. Every Ferrix target today has one last-level cache per
domain. A server that wakes many clients (wineserver) and a client of
such a server therefore stay where they are, and Linux's tie-breaks then
spread them.

The arithmetic (`wake_wide`, `flip`) lives in `ferrix-sched` and is
host-tested.

## 4. Rule C: a new task charges the processor it is placed on

`choose_cpu` is unchanged: idle first, then fewest runnable, then lowest
average, and `prefer` on a tie. What changes is that the first enqueue of
a newly made task adds the task's weight to its processor's load average
(`Load::charge`), in units of `LOAD_SCALE` per nice-0 weight. This is
Linux's initial load for a new entity. The charge decays like any other
load: half in 33 ms, and under 1% after about 230 ms. So the next
placement in a burst sees the processor it just used as busier, even when
the new task went to sleep at once.

- Applies to `prepare_user` (threads and processes) and to `spawn`
  (kernel threads), at `enqueue`, and only in a domain that steals work.
  A pinned creation there (`spawn_in_group`, `prepare_user` with `cpu`)
  charges too: the load is real wherever it lands, and Linux charges
  every new entity.
- The charge is added under the queue's lock, after the average has been
  brought up to now (`account_load`). Otherwise the long-gap path of
  `Load::accumulate` would overwrite it. The addition saturates.
- A burst of hostile creations can raise one domain's averages, but only
  for as long as the charge decays (C1).
- The balancer reads the same average. `imbalance` still needs a
  difference of two queued tasks before it moves anything, so a charge
  alone never moves a task.
- Exec of an existing task is not re-placed. Linux does re-place at exec
  (SD_BALANCE_EXEC). Here that is a later row.

## 5. What is measured before landing

- `efsched6` (fixed phase), three rotated rounds under
  `fleet/bench.lock`, with the load recorded: preemptions a second (mean
  and max spinner), light-wake p50 and p99, and throughput, against main
  and Linux 7.0. **Bound: p99 no worse than main's range.**
- A case where the woken task is heavy.
- `bench-ipc --alternate origin/main`, ABAB, median of 5 ratios. The
  earlier placement attempt took channel round trips from 729 to
  1,378 ns. Channels use Wake::Sync and are not reached by A or B. Bound
  (ledger 3062, Q3): both call and domain-call p50 at or under 1.05, at
  x86_64 kvm `--smp 1` and `--smp 4`, and on aarch64 `--smp 2`.

## 6. Requirements (reserved, `tools/common/data/requirement-reservations.json`)

- **L.sched.86** (parent H.SCHED.1): Rule A, the affine pull with its
  conditions, idle first, weight bound, and state 2 only.
- **L.sched.87** (parent H.SCHED.1): Rule B, no pull when the wake is
  wide.
- **L.sched.88** (parent H.SCHED.1): Rule C, a new task's weight charged
  to its processor's load average at its first enqueue.
- L.sched.89 is spare. It is released if unused.

Each gets a stage 5 self-check with three attempts:

- **86:** a waker in a call on A wakes a task homed on busy B, and the
  task runs on A. A pinned or home-idle wakee stays.
- **87:** the waker's flips are set wide, and the same wake stays at home.
- **88:** a burst of N sleeping tasks made from one processor, with every
  processor busy, is spread over at least two processors.

Each gets a negative control through `gate.sh control --expect`, with the
rule removed, and the case must fail with its own sentence. Host tests in
`ferrix-sched` cover `wake_wide`, the flip decay, the weight comparison
and `Load::charge`.

## 7. Review

The consultant's verdict is **OK IF (1)-(12)** (ledger line 3062,
2026-10-22). Q1: yes, with L.sched.86 stating the rule as built and a
heavy-wakee case. Q2: yes, with the charge made after `account_load` and
read by no real-time path. Q3: at or under 1.05 (below). The idle-first
order in §2.2 is condition (2). Wake::Sync keeps its own path, which is
condition (1).

The questions as asked:

1. Is the weight rule acceptable without a per-task load term (§2.3)?
2. Is charging the load average acceptable, given that it is the
   balancer's input too (§4)?
3. Is the bound for IPC the earlier one (channel round trip within
   ±5% of main at `--smp 1` and `--smp 4`)?
