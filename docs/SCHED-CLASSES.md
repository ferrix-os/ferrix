# Scheduling classes as removable crates

**Design reviewed, 2026-10-26: OK IF S1-S14** (§10; ledger line 11 of
this container's consultant ledger, still to be copied to the gate
host's). Not built. The customer asked
(2026-10-26): "design all our schedulers to be crates with one common
compile-time trait interface between the kernel, so that we can select our
desired schedulers by enabling or disabling a crate via a flag … and that
certification which requires no dead code can easily be achieved by just
moving out the crate". This is that design, for stage 14
([REALTIME.md](REALTIME.md) §3) and MICROKERNEL.md's scheduler flags. It
needs the certification consultant's design review before any code, as
every composition-root seam has had (KERNEL-CRATES.md §6.3).

## 1. The answer in one paragraph

Each scheduling class -- fair (EEVDF), fixed priority (FIFO/RR), deadline
(EDF with a hard CBS), and later the static time windows -- becomes a crate
of its own under `src/lib/kernel/`, implementing one trait,
`SchedClass`, from a small interface crate. Which classes a kernel
contains is chosen at compile time by build flags in the profile files,
one per class. A class that a profile turns off is never named by the
binary, so rustc never loads it and the linker never sees it: absent code,
neither dead nor deactivated (DO-178C §6.4.4.3). Inside a kernel, which of
the compiled classes a domain *uses* is its mode, chosen at run time, and
a mode whose classes are not compiled is refused, as `domain.rs` refuses
`SoftRt` and `HardRt` today. Dispatch is static: no `dyn`, no function
table, nothing registered at run time. A certified profile fixes one
implementation a tier, monomorphised and inlined (§5); the desktop profile
may compile several and switch among them at run time through the same
trait (§5a).

## 2. Why build flags and not Cargo features

The request names a flag in `Cargo.toml`. The tree already has a reviewed
rule against that, and a mechanism that does the same job with stronger
evidence:

* **The rule.** BUILD-FLAGS.md §2 rule 1 and KERNEL-CRATES.md §6.1 (6):
  "A flag is never a Cargo feature (`kernel_cargo_features` stays 0)". The
  certification item's reference configuration says why: with no Cargo
  features in the kernel there is "no Cargo configuration space to
  enumerate", which is most of what makes the item analysable. Features
  are additive and unified across the dependency graph, so a feature a
  test or tool turns on can silently put a class into a kernel that did not
  ask for it.
* **The mechanism.** A build flag lives in
  `tools/common/data/build-flags.toml`, each profile
  (`tools/common/data/profiles/*.toml`) sets it explicitly, xtask passes it
  to rustc as a `--cfg`, and only the binary's root names a removable
  crate under that cfg. `runtime_settings` already removes
  `ferrix-kernel-settings` this way (KERNEL-CRATES.md §6, built
  2026-10-24).
* **What "moving the crate out" becomes.** Setting the class's flag to
  `absent` in the profile. Cargo still lists the crate as a dependency in
  every build (it cannot make a dependency conditional on a build-script
  cfg without a feature); the evidence that a kernel contains no code of an
  absent class is the linked ELF, checked by `check-removable-symbols.py`,
  which is stronger evidence than a `Cargo.toml` line, because it is
  what actually runs.

If the customer wants Cargo features anyway, that reverses ledger 3558
(10); it is the consultant's to rule on first, and §8 lists what it would
cost.

## 3. The crates

| Crate | Ring | What it holds |
|---|---|---|
| `ferrix-sched-class` | core, `#![forbid(unsafe_code)]` | the `SchedClass` trait, the shared types (`TaskRef`, `Instant`, `Nanos`, `Refused`, `Charged`), the class-stack combinator, and nothing else |
| `ferrix-sched-fair` | core | EEVDF: today's `ferrix-sched` run queue, weights and `carried_weight`, moved |
| `ferrix-sched-fifo` | core | `SCHED_FIFO` and `SCHED_RR`: 99 priority lists and a bitmap, the RR quantum, the deferred fair server |
| `ferrix-sched-edf` | core | partitioned EDF with a hard CBS: reservations, refills, per-CPU admission |
| `ferrix-sched-windows` | core | later (M3): the static time-partition schedule of MICROKERNEL.md K9, with fixed-priority preemptive scheduling inside each window (C6) |
| more, later | none until evidenced | BORE, a userspace `sched_ext`-style agent, and others: each a crate and a flag value (§6a) |
| idle | in the kernel | always present: the bottom of every stack, not a crate |

The class crates are pure: no `unsafe`, no `ferrix_` cfg (BUILD-FLAGS.md
rule 6 b already fails a `src/lib` crate that names one), no allocation
after construction, and each is host-tested on its own with the same
property tests `ferrix-sched` has. Each depends on `ferrix-sched-class` and
nothing of the kernel. That gives each class its own certification unit:
its own requirements, its own coverage, its own level in
`certification-item.json`, and a profile that does not link it carries
none of its evidence. This also retires F-76 for the scheduler (today
`ferrix-sched` is an unclassified dependency).

## 4. The trait

As built (2026-10-26, `src/lib/kernel/sched-class/src/lib.rs`), with the
review's S5 and S11 in it. Generic over the payload `T` the kernel queues
(its `Arc<Task>`), associated functions over a passed-in queue:

```rust
pub trait SchedClass<T> {
    const NAME: &'static str;
    type Queue;            // what one CPU keeps
    type Entity;           // what a task carries off every queue
    type Config: Copy;     // what a queue is made with

    fn new_queue(config: Self::Config) -> Result<Self::Queue, Refusal>;
    fn len(queue: &Self::Queue) -> usize;
    fn enqueue(queue: &mut Self::Queue, id: u64, payload: T, entity: Self::Entity)
        -> Result<(), Back<T, Self::Entity>>;
    fn remove(queue: &mut Self::Queue, id: u64) -> Option<(T, Self::Entity)>;
    fn remove_current(queue: &mut Self::Queue) -> Option<Left<T, Self::Entity>>;
    fn pick(queue: &mut Self::Queue) -> Option<&T>;
    fn put_back(queue: &mut Self::Queue);          // a stack's lower tier, preempted
    fn current(queue: &Self::Queue) -> Option<&T>;
    fn hand_over(queue: &mut Self::Queue, id: u64, payload: T, entity: Self::Entity,
        slice_after: u64) -> Handed<T, Self::Entity>;   // required (S5)
    fn charge(queue: &mut Self::Queue, ran_ns: u64) -> bool;
    fn remaining_ns(queue: &Self::Queue) -> Option<u64>;
    fn steals_work(queue: &Self::Queue) -> bool;   // a function of the queue (S11)
    fn check(queue: &Self::Queue) -> Result<(), &'static str>;
}
```

`Back` hands a refused entity back whole with a `Refusal` (`Invalid`,
`Duplicate`, `Unschedulable`, `NoSuchTier`); `Left` is a running entity
taken off. `Over<U, L>` stacks two classes and is itself one, its entity a
`Tier<U, L>`; `Empty` is the class with no entities (its entity type is
`Infallible`). The fair class implements it as `ferrix_sched_fair::Eevdf`,
forwarding to `RunQueue`.

**Still to come**, each with the class that needs it and its own review:
`admit` (the deadline class's test), the policies a class takes (§6a),
`boost` and `unboost` for inheritance (defined at the stack level, C2 of the
design review), and the operations the core still makes on the fair queue
directly (`core/src/sched/stack.rs` lists them): a task's weight, the
picture `/proc` reads, the slice, the processor's load, the stealing
choice. The trait has no provided methods yet, so S11's check of inherited
defaults applies from the first one.

## 5. How the kernel reaches it: a selection crate, chosen before the core

The scheduler is called from everywhere in the core: the wake paths, the
futex and address-wait tables, IPC's direct switch, the timer. A first
draft of this design bound it at link time, as `Setters` is
(KERNEL-CRATES §6.3, "a caller that cannot name it"): the core calling
`__ferrix_sched_<op>` symbols the binary defines. That is static but not
free: every scheduler operation becomes an out-of-line call across a crate
boundary that rustc cannot inline (fat LTO in `release` may recover it;
`dev` and `iterate` do not), on the IPC round trip's path. The customer
asked for compile-time traits with no run-time cost, so the choice moves
*before* the core instead:

1. **`ferrix-kernel-sched-select`**, a kernel crate of a few lines, sits
   below the core in the crate graph. It depends on the interface crate and
   on every class crate, and holds one item, chosen by the build flags:
   ```rust
   #[cfg(ferrix_sched_dispatch = "fixed")]
   pub type Stack = /* the profile's classes, e.g. */
       Over<ferrix_sched_windows::Arinc, ferrix_sched_edf::EdfCbs>;
   #[cfg(ferrix_sched_dispatch = "switchable")]
   pub type Stack = ferrix_sched_switch::Switchable<DesktopSet>;
   ```
   Written by hand, not generated (S2): one cfg arm a declared value of
   each tier flag, giving that tier's type, an absent tier being a
   zero-sized `Empty` class; `check-build-flags.py` holds the arms one to
   one with the flag file.
2. **The core names `Stack` directly**: `Stack::pick(..)`, `Stack::charge(..)`.
   rustc monomorphises and inlines it into the core in every profile, dev
   included: no symbol, no `unsafe`, no registration. A crate the flags do
   not select is named nowhere, so rustc never loads it and the linker
   never sees it, as before. The type checker is the registration check:
   a profile whose flags select no `Stack`, or two, does not compile.
3. **What it asks of the rules** (S2, accepted over the link-time seam).
   BUILD-FLAGS.md rule 6e allows a recorded flag's cfg only in `main.rs`,
   `build_flags.rs` and the core's flag word. The exception is one file:
   this crate's root, a kernel crate in the core ring under `src/kernel`,
   built by the one build script, its flag word const-asserted equal to the
   binary's (K4). Nothing else in the kernel reads a scheduler flag: which
   modes exist, the boot line and the policy lists come from `Stack`'s
   constants. `removable_problems` cannot hold these crates as it stands
   (it requires a dependency on the load crate and the binary as the only
   dependant), so the class crates get a record kind of their own, with its
   self-tests and controls (S3).
4. **Per-task state lives in `Task`** (S4). The core names `Stack`
   concretely, so `Task` holds `<Stack as SchedClass>::Entity` and the
   task's `Params` as fields, allocated and charged to the job with the
   `Task` as today. Its layout differs by profile, which is fine: each
   profile is its own reference configuration. (A first draft kept a
   boot-sized table indexed by task slot; the review refused it: there is
   no system-wide task limit to size it by, and it would add a global
   refusal and a charging question F-37 would have to answer.)

## 5a. Fixed for the certified builds, switchable for desktop

The customer (2026-10-26): "Ideally for the desktop build it should be
dynamic, while for the max security build it should be fixed in place
compiled in." One flag, **`sched_dispatch`**, says which:

| Value | `Stack` is | Allows | Profiles |
|---|---|---|---|
| `fixed` | the profile's tiers, stacked with `Over` and monomorphised: one implementation a tier, no run-time choice | `sc4`, `present`, `sc2`, `none` | `max`, `fast`, `present` |
| `switchable` | `Switchable<Set>`: every implementation the profile compiles, a run-time choice a tier per domain | `none` | `desktop` |

**`Switchable` is one more implementation of `SchedClass`**, in its own
removable crate, `ferrix-sched-switch`, so the core is the same code in
both: it calls `Stack::pick` either way. `Switchable<Set>` holds, a tier, an
enum of the implementations the desktop profile compiles (say
`Fair::{Eevdf, Bore}`, `Realtime::{FifoRr}`, `Deadline::{EdfCbs}`) and,
a domain, which variant a tier uses now. Each operation is a `match` on
that variant forwarding to the chosen crate's function, monomorphised: no
`dyn`, no function pointers (the trait's associated types do not allow
trait objects anyway), one well-predicted branch a call, since the choice
changes rarely. A `max` kernel never links `ferrix-sched-switch` or any
implementation it does not stack, so it carries no switching code and no
alternative scheduler: absent, not deactivated.

With `switchable`, a tier flag's value becomes a set (`sched_fair =
["eevdf", "bore"]`): the compiled implementations the domain may choose
among; with `fixed` it must be one value, which `check-build-flags.py`
holds. The profile rule against deactivated code still applies: every
compiled implementation must be one a domain may select.

**The run-time switch.** A domain changes a tier's implementation through
the same handle that changes its mode (GAME-PATH's session-held domain
handle), with a right of its own, and the same quiescent protocol as a
mode switch (REALTIME.md §3.7), because a scheduler's per-task state means
nothing to another (EEVDF's lag is not BORE's burst score nor EDF's
deadline):

1. **Prepare**: make the new implementation's per-CPU queues for the
   domain's CPUs, before anything stops; with no memory, refuse.
2. **Stop admitting** into the tier on those CPUs.
3. **Quiesce**: every CPU of the domain passes a scheduling point (an IPI
   rendezvous); `smp::synchronize` lets lock-free readers of the old
   variant finish.
4. **Move the tasks**: each task of the tier is dequeued from the old
   implementation and enqueued into the new one with fresh per-task state
   from its `Params`; a task whose `Params` the new implementation does not
   accept is demoted with an error the switching caller sees, never
   silently (ARCHITECTURE.md §5).
5. **Swap** the domain's variant in one store, **resume**, and free the old
   queues after another grace period.

**Where the switching lives** (S9, S10). The protocol needs `smp` and the
IPI rendezvous, which a crate below the core cannot call, so the crate
graph is drawn acyclic: `Switchable` and its per-domain variant, the
moving of tasks with fresh state, the run-time making of queues, and the
right and the entry (native or Linux) that asks for a switch all live in
`ferrix-sched-switch` or a removable kernel crate above the core. In a
`fixed` build that entry answers `NOT_SUPPORTED` at its dispatch, before
any argument check. No `Mode` variant, trait method or `Domain` field
exists only for switching. `switchable` allows level `none` alone, every
profile with a claim is `fixed` (so `fast` stays `fixed`), and a `fixed`
profile may not give a tier a set of values; each rule has a control.

No guarantee holds across the switch, and the transition is bounded by the
longest non-preemptible section, the IPI latency and the number of tasks
moved; the desktop has no claim to keep, which is why this is the desktop's
only.

**Loading a scheduler that was not compiled in** is a different thing.
Linux does it with `sched_ext` and BPF; Ferrix has neither BPF nor loadable
kernel modules. On desktop it is the `ext` tier's userspace agent (§6a),
which `switchable` can select like any other implementation, with its
watchdog falling back to the fair tier. A verified in-kernel bytecode for
policies would be a project of its own, and never in a certified profile.

## 6. Flags, modes and profiles

* **One flag per tier, whose value names the crate that fills it.** A
  *tier* is a place in the stack, in Linux's order: `windows` (the static
  time partitions, outermost), `deadline`, `realtime` (fixed priority),
  `fair`, `ext` (an extensible, agent-driven scheduler, §6a), then idle,
  always there. The flags are `sched_windows`, `sched_deadline`,
  `sched_realtime`, `sched_fair` and `sched_ext`, and each value is a crate
  or `absent`, declaring the highest level it allows as every flag value
  does. They replaced the `schedule` flag (`fair` | `edf` | `windows`) in
  step 3a, with `sched_dispatch` (§5a); `sched_ext` waits for ext's design
  (a flag has two values at least, §9a). Today's values, with room for
  more (§6a):

  | Flag | Values |
  |---|---|
  | `sched_windows` | `arinc` (`ferrix-sched-windows`), `absent` |
  | `sched_deadline` | `edf_cbs` (`ferrix-sched-edf`), `absent` |
  | `sched_realtime` | `fifo_rr` (`ferrix-sched-fifo`), `absent` |
  | `sched_fair` | `eevdf` (`ferrix-sched-fair`), `absent` |
  | `sched_ext` | `absent` |

  | Profile | windows | deadline | realtime | fair | ext |
  |---|---|---|---|---|---|
  | `max` | `arinc` | `edf_cbs` | `absent` | `absent` (C3, decided) | `absent` |
  | `fast` | `absent` | `edf_cbs` | `fifo_rr` | `eevdf` | `absent` |
  | `desktop` | `absent` | `edf_cbs` | `fifo_rr` | `eevdf` | `absent` |
  | `present` (today) | `absent` | `absent` | `absent` | `eevdf` | `absent` |

* **Modes are the run-time choice among compiled tiers.** `Throughput`
  needs the fair tier; `SoftRt` needs the realtime and fair tiers; `HardRt`
  needs the deadline tier. A mode names tiers, never crates, so it does not
  change when a tier's crate does. A domain
  asked for a mode whose classes are absent is refused `ModeUnavailable`,
  exactly as today. `Mode::classes()` becomes a `const fn` of the compiled
  flags, so `SoftRt` in a kernel without fifo is a refusal the compiler
  can see.
* **No deactivated code either.** "Absent" covers code not linked; code
  linked but never reachable in a configuration is *deactivated*, and needs
  its own argument. So the profile rule is two-sided: every class a profile
  marks present must be used by at least one mode the profile allows, and
  every mode it allows must have its classes present. `check-build-flags.py`
  enforces both, with a planted profile of each kind as its negative
  controls.

## 6a. Open for schedulers added later

The set of schedulers is meant to grow: BORE, `SCHED_RR` on its own,
`sched_ext`-style schedulers and whatever comes after. Nothing in the design
names a fixed list, and adding one is a crate, a flag value and a binding
line, never a change to the core.

**Adding a scheduler** is these steps, and only these:

1. A crate under `src/lib/kernel/` implementing `SchedClass`, pure as the
   others (§3): no `unsafe`, no `ferrix_` cfg, host-tested on its own.
2. A value of its tier's flag in `build-flags.toml`, naming the crate and
   the highest level it allows. A new scheduler starts at `none` (no claim)
   and rises only with its own evidence, so adding one can never weaken a
   certified profile: a certified profile names its values explicitly
   (BUILD-FLAGS.md rule 4) and does not pick up a new one by default.
3. Its cfg arm in the selection crate (and its place in `Switchable`'s
   set, if desktop compiles it), and one entry in the class crates' record
   (S3), which the checks of that record kind, the ELF and link-line checks
   and `test-removable` then cover.
4. Its Linux policy numbers declared (below), if it takes any.

**A tier holds one crate per build**, chosen by its flag, so two
implementations of the same tier are alternatives: BORE or EEVDF, never
both picking for one CPU. A later step may let one build hold two crates
in a tier for different domains (EEVDF for a server domain, BORE for a
desktop one): the flag's value becomes a set, the domain's mode names
which, and the profile rule still holds every compiled crate to a mode
that uses it. That is a change to the flag and to `Mode`, not to the trait.

**The trait stays stable as it grows.** The operations every scheduler
needs are required methods. An operation added later (a new hint, a new
accounting event) arrives as a provided method whose default refuses or
does nothing, so existing crates still compile. A profile that claims a
level then requires every crate it links to implement each such method
itself, which a check reads from the crate (a default inherited in a
certified crate is a decision nobody made). Each crate also declares what
it is, as associated constants the stack and the checks read:

```rust
/// Which native policies a task may ask this crate for. The Linux
/// personality maps its policy numbers (SCHED_OTHER 0, FIFO 1, RR 2,
/// BATCH 3, IDLE 5, DEADLINE 6, EXT 7) to these in its own table (C1).
const POLICIES: &'static [Policy];
/// Whether `admit` can refuse: true for a class with a guarantee to keep.
const ADMITS: bool;
```

The Linux personality's `sched_setscheduler` and `sched_setattr` look a
policy up in the compiled tiers' `POLICIES`, and a policy no compiled crate
declares is `EINVAL`, as `SCHED_FIFO` is today. So `SCHED_EXT` exists in a
kernel exactly when an ext crate does.

**The examples the customer named:**

| Scheduler | Tier | How it fits |
|---|---|---|
| **BORE** (Burst-Oriented Response Enhancer, a patch on Linux's EEVDF that weights a task by how bursty it has been, for desktop responsiveness) | `fair` | `ferrix-sched-bore`, a value `bore` of `sched_fair`. It shares most of EEVDF's code, so the fair crates should keep the EEVDF run queue in a library both use, with BORE adding its burst score to the eligibility and the slice. `desktop` could choose it; a certified profile would not without its own evidence. |
| **`SCHED_RR` on its own** | `realtime` | `fifo_rr` already serves both policies (one priority bitmap; RR adds a quantum on the CPU's one-shot timer). If RR alone is wanted, a crate `ferrix-sched-rr` declaring only policy 2 is another value of `sched_realtime`. |
| **`sched_ext`** (Linux 6.12: a scheduler written in BPF and loaded at run time, with a watchdog that falls back to the fair class when it stalls) | `ext` | Ferrix has no BPF, so the policy runs as a **userspace agent**: `ferrix-sched-agent`, a value `agent` of `sched_ext`, keeps a per-CPU queue the agent fills through a native channel and shared ring (Google's ghOSt is the same shape), and a watchdog that hands the agent's tasks back to the fair tier if the agent misses a deadline or dies, as `sched_ext`'s does. Policy 7 maps to it. It is the clearest case for "absent" in a certified profile: a policy loaded at run time is code no certification has seen. |
| **LAVD** (`scx_lavd`, Latency-criticality Aware Virtual Deadline: a `sched_ext` scheduler from Igalia, made for gaming on the Steam Deck *(unverified details)*. It infers how latency-critical a task is from how often it wakes others and is woken, so the tasks on a frame's producer-consumer chain get earlier virtual deadlines; it also compacts work onto fewer cores when that saves power) | `fair` | `ferrix-sched-lavd`, a value `lavd` of `sched_fair`, desktop only, selectable beside EEVDF through `Switchable`. A **native reimplementation in Rust**, not a port: Ferrix has no BPF, and `scx_lavd` is GPL-licensed *(unverified: GPL-2.0)* while Ferrix is MIT, so it is written from the published description of the algorithm (its README, the authors' talks and papers), never from its source. Its inputs are the wake and wait events every class already sees, plus a per-task wake-frequency record in its `Entity`. Measured against EEVDF and BORE on the game path (GAME-PATH.md's frame-time rows) before it is offered. Core compaction needs the power code to know which cores are parked, which is outside the trait and a later step. |
| **CFS, a classic O(1), MuQSS, …** | `fair` | Each another value of `sched_fair`, for comparison or for a workload. |
| **Static priority ceilings, a mixed-criticality class, a cyclic executive for a domain** | `deadline` or `windows` | Further values of those tiers. |

**What stays fixed** is the trait's shape (per-CPU queue state, per-task
state, admit, enqueue, dequeue, pick, charge, preempts, next event, check,
boost) and the tier order. A scheduler that needs something none of these
can say -- say, gang scheduling across CPUs -- needs a new operation in the
trait, added as above, with its own review.

## 7. Decisions for the customer and the consultant

1. **Cargo features or build flags** (§2). This design uses build flags;
   Cargo features need ledger 3558 (10) reversed.
2. **Is fair optional?** A `max` kernel with only `edf` and `windows`
   means every thread, the kernel's own workers and every server included,
   runs on a reservation. That is the strongest claim and the most work: no
   best-effort task can exist. The alternative keeps fair as the lowest
   class in every profile, inside EDF's idle time only on `HardRt` CPUs. **Decided 2026-10-27: no fair in `max`**
   (§7a, C3).
3. **Where SoftRt's fair server lives**: in `ferrix-sched-fifo` (it exists
   only to protect fair from fifo) or as a small `ferrix-sched-server`
   crate that EDF could reuse.
4. **The selection crate's cfg exception** (§5.3), or the link-time
   binding with its cost.
5. **Switchable for desktop** (§5a): accepted as the desktop's only, with
   the switch protocol; and whether `fast` stays `fixed`.

## 7a. Settled by the review

* Build flags, not Cargo features: ledger 3558 (10) stands. The customer's
  "flag in our Cargo.toml" is one line of a profile file
  (`sched_realtime = "absent"` in `tools/common/data/profiles/max.toml`
  leaves the crate out).
* The selection crate over the link-time seam (S2); per-task state in
  `Task` (S4).
* `fast` stays `fixed`; `switchable` is desktop's alone (S10).
* **C3, decided by the customer (2026-10-27): no fair tier in `max`** --
  "i would keep max to the absolute minimum, so: no". `max` is the windows
  tier alone: every kernel worker and server runs on a reservation, and
  every user of the fair crate outside `Stack` (§11's table, the `NICE_`
  weights among them, in the core, the item and the load) must go through
  `Stack` or be absent before `max` can build.
* **C6, decided by the customer (2026-10-27): fixed priorities inside the
  windows.** Inside each partition's window `ferrix-sched-windows`
  schedules by fixed-priority preemption, as ARINC 653's APEX and the
  certified partitioning RTOSes do (PikeOS, VxWorks 653, INTEGRITY-178),
  analysed by response-time analysis, which assessors know; not EDF, as
  this design first said. An overload in a window then misses the lowest
  priorities' deadlines first, not every task's. `max`'s real-time
  scheduling is this tier alone: `sched_realtime` (FIFO/RR without
  budgets) and `sched_deadline` stay absent there. Where the fair server
  lives is free (C4: a crate of its own only once two classes link it).

## 8. What it costs, and what it would cost with Cargo features

**This design:** moving today's scheduler behind the selection crate is a refactor of
the core with no behaviour change (fair alone, the `present` profile),
landed and gated before any new class, as `Setters` was. Then each new
class is a crate, its flag, its binding line and its checks. Extending
`removable_problems`, `check-removable-symbols.py` and `test-removable` to
the four new crates is configuration of checks that already exist.

**With Cargo features instead:** a `[features]` table in the kernel's
manifest with one feature per class and `dep:` optional dependencies;
`cfg(feature = ...)` in the root in place of the flag cfgs; the profiles
translated into `--features` lists by xtask; and new checks that no test,
tool or workspace member unifies a feature into a certified build, plus a
rewrite of `kernel_cargo_features`'s rule and of the item's reference
configuration. It would put the crate's absence in `Cargo.toml`, as asked,
at the price of the configuration space the item was built to avoid.

## 9. Order

1. This design, reviewed; requirement ids reserved on `develop`.
2. `ferrix-sched-class` and the selection crate, with `ferrix-sched` moved
   to `ferrix-sched-fair` behind it, `fixed`. No behaviour change; the IPC
   round trip measured before and after.
3. The per-class flags replacing `schedule`, with the profile rules and
   their controls.
4. Then stage 14's classes, each as its own crate and landing, in
   REALTIME.md §4's order: `ferrix-sched-fifo` with SoftRt, then
   `ferrix-sched-edf` with HardRt, then `ferrix-sched-windows` with M3.
5. `ferrix-sched-switch` and `sched_dispatch = "switchable"` for desktop,
   once there are two implementations of a tier to switch between (BORE
   beside EEVDF, or the `ext` agent).

## 9a. Step 3, as its design review has it (OK IF D1-D13, 2026-10-27)

A design review before code, ledger line 20 of the container's ledger (to
be copied to the gate host's). Step 3 is split: **3a**, the flags, the
selection crate, the profiles, the audit words and the modes; **3b**, S3's
record kind for the class crates and S1's absence evidence. 3a may land
before 3b (D11).

**The flags** (D1). `schedule` goes; six flags take its place, in this
order, each `absent` value allowing every level (in a claimed build S1's
evidence is what makes absence true, D11):

| Flag | Values (allows; built) |
|---|---|
| `sched_dispatch` | `fixed` (sc4, present, sc2, none; yes), `switchable` (none; no, step 5) |
| `sched_windows` | `arinc` (windows's: sc4, present, sc2, none; no, M3), `absent` |
| `sched_deadline` | `edf_cbs` (edf's: present, sc2, none; no, M3), `absent` |
| `sched_realtime` | `fifo_rr` (none; no, stage 14), `absent` |
| `sched_fair` | `eevdf` (fair's: present, none; yes; `present` standing under F-76 until S7 classifies ferrix-sched-fair), `absent` |
| `sched_ext` | `absent` |

A value that directly succeeds a recorded one keeps its allows; one with
no predecessor starts at `none`.

**The profiles** (D1, D2), today's `schedule` translated value for value:

| Profile | dispatch | windows | deadline | realtime | fair | ext |
|---|---|---|---|---|---|---|
| `present` | fixed | absent | absent | absent | eevdf | absent |
| `desktop` | fixed | absent | absent | absent | eevdf | absent |
| `fast` | fixed | absent | edf_cbs | absent | absent | absent |
| `max` | fixed | arinc | absent | absent | absent | absent |

`max`'s scheduling inside its windows is the windows crate's own,
fixed-priority (C6, §7a), not a deadline tier; its `fair = absent` translates `schedule = "windows"`,
and is what the customer decided for C3 (§7a). `max` and `fast` cannot
build, as today, for their unbuilt values and, besides, while the core
uses fair outside `Stack` (§11's table). `desktop` stays `fixed` until
step 5; `fifo_rr` and `switchable` enter profiles with their crates.

**Composition** (D3). One generic alias a tier flag, a value an arm,
`absent` the identity:

```rust
#[cfg(ferrix_sched_windows = "absent")]  pub type Windows<L> = L;
#[cfg(ferrix_sched_windows = "arinc")]   compile_error!("sched_windows=arinc is not built yet");
// likewise Deadline<L> and Realtime<L>
#[cfg(ferrix_sched_ext = "absent")]      pub type ExtUnder<C> = C;
#[cfg(ferrix_sched_fair = "eevdf")]      pub type Fair = ferrix_sched_fair::Eevdf;
#[cfg(ferrix_sched_fair = "absent")]     pub type Fair = ferrix_sched_class::Empty;
#[cfg(ferrix_sched_dispatch = "fixed")]
pub type Stack = Windows<Deadline<Realtime<ExtUnder<Fair>>>>;
```

The fair-only stack is exactly `Eevdf`: N2's const holds unchanged, no
`Empty` tier nests, and `Over` costs only where two tiers are present. A
built value's arm becomes `Over<Its, L>`. Every built-no value's arm is a
`compile_error!` naming it, besides xtask's refusal (D7). A const asserts
the stack's name is not empty (line 15's C1). S5's evidence: the release
ELF table (`choose_next`, `ferrix_fast_write_read`) before and after on
all three architectures, and the round trip on the landing hash with line
13's. If the table shows `Over`'s stored `running_upper` costing, it is
derived from `U::current` instead (C2).

**The selection crate** (D4): `ferrix-kernel-sched-select`,
`src/kernel/sched-select/Cargo.toml`, built by the one build script, its
root `src/kernel/src/sched_select.rs` the arms file. A `kernel_crates`
entry, ring core, in `certification-item.json`; `unclassified_deps`
ferrix-sched-class and ferrix-sched-fair (F-76's register grows under line
20); a layering rule that it links none of ferrix-kernel, -core, -item,
-load or ferrix-boot; check-item-boundary passing; only crates that name
it depend on it. `build_flag_word!` and `flag_word` move into it, one
list, the core re-exporting both, and the binary asserts its word equal to
the others (K4's fifth).

**The flag word** (D5). `flag_word` fails at compile time when given more
than 64 values (today it drops them, so two crates built with different
late flags would compare equal). With step 3 it holds 52 of 64;
BUILD-FLAGS.md records the headroom.

**Who reads the flags** (D6). The arms file is the only file that selects
code by a `sched_*` cfg. The boot line's words and `audit_words`' sched
fields come from value-name consts each arm defines, not from `cfg!` in
`build_flags.rs`. Rule 6e's check allows a `sched_*` cfg only in the arms
file and in `build_flag_word!`'s definition; controls, each `FIRED`: a
planted `cfg!(ferrix_sched_fair = ..)` in a core file, a declared value
with no arm, a value with two arms.

**The audit words** (D8, S8). The first word: the fourteen other flags,
two bits each. The second: the profile in bits 0-3, the level in 4-7,
`sched_dispatch` in 8-9, the five tiers three bits each in 10-24 (eight
values a tier). AUDIT.md §1 is restated with each list's decoding; H.AUD.7
and its check stand, no id. BUILD-FLAGS.md, REALTIME.md, MICROKERNEL.md
and this file stop naming `schedule`. The `build` line changes on every
boot, so the four boots' rows show it with check-boot-line passing.

**Modes** (D9, S12). `Throughput` uses the fair tier; `SoftRt` the
realtime and fair tiers; `HardRt` the deadline tier **or the windows
tier**, whose crate schedules by fixed priority inside its windows (C6),
a hard real-time mode by its reserved windows, so `max`
allows `HardRt` and no new mode is made. No mode uses `ext` until ext's
own design says one does (and where ext sits, under fair or as the fair
tier, is that design's: C1). A profile with both the deadline and the
windows tier runs both under HardRt, as intended: the windows partition
time between domains, and the deadline tier is the EDF of a domain whose
processors are its own, outside the windows; no profile has both today,
and the one that first does says which domains each serves (ledger line
21, C1). `Mode` lives below the selection crate, so
availability is a const of the selection crate the domain's refusal
consults. Profiles gain a `modes` key; check-build-flags holds it equal to
the set of modes whose tiers are present, both sides with a control: a
mode allowed whose tiers are absent, and a mode whose tiers are present
left out. No id while every buildable profile allows only `Throughput`;
the first that allows another reserves one.

**Dispatch** (D10, S10). check-build-flags fails a claimed profile whose
`sched_dispatch` is not `fixed`; the control, `switchable` planted in
`fast.toml`, reads `FIRED`. A tier given a set of values cannot arise
while each tier flag takes one value.

**3b and the order** (D11). S3's record kind with its self-tests and
controls, and S1's link-line and ELF evidence with a planted
unconditional use reading `FIRED` (a fixture class crate if no real absent
crate exists). 3b lands before any class crate other than fair enters the
tree, and before any profile with an absent tier whose crate is in the
tree builds or claims. The `classes` boot line's comparison with the
profile, and its requirement from L.sched.102, come with 3b.

**Ids** (D12): none for 3a.

## 9b. Step 3b, as its design review has it (OK IF G1-G10, 2026-10-27)

A design review before code, ledger line 22 of the container's ledger (to
be copied to the gate host's): S1's absence evidence and S3's record kind.

**Why a fixture** (G1). No buildable profile can leave a class crate out
before step 4: `sched_fair = "absent"` cannot build while the core uses
EEVDF outside `Stack` (§11), and a second, present class cannot build
while the core makes EEVDF-only calls on the queue (`stack.rs`). So the
absent side is shown with `ferrix-sched-fixture`
(`src/lib/kernel/sched-fixture`): a minimal FIFO class, `no_std`,
`#![forbid(unsafe_code)]`, `publish = false`, no `ferrix_` cfg, host
tested, declaring no flag value. It is a dependency of the selection crate
alone and named by no file, so rustc never loads it and the linker never
sees it: S1's claim, on a real build. It stays after step 4 as the
standing negative case, a crate no value links in any profile (C1), until
a ledger line retires it.

**The unused dependency** (G2). The workspace warns on
`unused_crate_dependencies`, and `use ferrix_sched_fixture as _;` would
load the crate and hand its rlib to the linker, the opposite of the
point. The arms file carries a crate-level `expect` for that one lint,
naming the fixture, and rule (d) below guards every other name of it.

**The record** (G4, G5): `class_crates` in `certification-item.json`, one
entry a class crate: its package, its Rust name (the symbol prefix), its
tier flag and the value that links it (`present_when`), or
`fixture: true`; its marker symbol (`<..Eevdf as ..SchedClass<..>>::check`).
check-item-boundary holds it beside `removable_problems`, each rule with a
self-test:
* (a) every crate under `src/lib/kernel/sched-*` that implements
  `SchedClass` has an entry (a class crate elsewhere, or an `impl` whose
  generic bounds take their own angle brackets, is not seen: class crates
  live under that path, and a reviewer reads a new one's `impl`; ledger
  line 23, C1), the interface crate `ferrix-sched-class`
  excepted (it implements it for `Empty` and `Over`), and every entry's
  package exists;
* (b) an entry's flag and value are declared, and the arms file's arm for
  that value names the crate's path;
* (c) of the kernel's crates (core, item, load, binary) only the selection
  crate depends on a class crate, but for the entries of `named_outside`,
  each naming its row of §11's N7 table (today `ferrix-sched-fair` in the
  core, the item and the load); the list shrinks freely and grows only
  with a ledger line;
* (d) no file names a fixture crate.

**The evidence** (G3, G8). `cargo xtask test-removable` gains a
class-crate half, per architecture, for `present` and `desktop`. The
release profile links with fat LTO and one codegen unit, so no Rust rlib
reaches the link line or a map of it, present or absent. So:
* the **ELF half** reads the shipped release ELF (`llvm-nm -C`): no symbol
  of an absent or fixture crate, the marker of each present one;
* the **link half** reads the linker map of a link of the same
  architecture, profile and flags with LTO alone turned off: neither a
  member of an absent or fixture crate's rlib nor a symbol of it; the
  present crate's code there, as a member of its rlib or, for a crate of
  generics alone such as `ferrix-sched-fair`, as its functions
  monomorphised into the crates that use it (the first x86_64 run found
  fair has no member of its own: 42 of its functions in the core's).
LTO only removes and merges code; it adds none from a crate it was not
given. So what the non-LTO link leaves out, the LTO image cannot hold, and
S1's "link line" (written as if there were no LTO) reads as this
non-LTO link. KERNEL-CRATES.md §6 states the same argument. The ELF and
the map are kept per profile and architecture beside test-removable's.
check-removable-symbols judges both, in a class-crate mode, so
test-removable keeps one judge.

**Controls** (G6), each with gate.sh, `FIRED`: (i) a planted unconditional
use of the fixture through the arms file, with the ELF and map judge run
alone on that build and `--expect` on its own message (rule (d) catches
the plant statically first, and has a control of its own); (ii)
`#[inline(always)]` on `Eevdf::check`, firing on the present side; (iii) a
`present_when` the arm does not link, firing statically.

**The ids** (G7), reserved in `requirement-reservations.json` before they
are written, released at landing. Absence is configuration evidence, as
KERNEL-CRATES §6's removable crates are, and takes no id. The classes line
is item behaviour:
* **H.SCHED.18**: "A kernel shall run the scheduling classes its build
  flags select and no other, and shall name them as it starts." Its
  criterion names test-removable's class-crate half on every architecture
  and buildable profile, and L.sched.102's check.
* **L.sched.102**, unit `sched::init`: "Once every processor is in the
  scheduler, sched::init shall print `sched    classes` and the names of
  the classes the build flags compose, outermost first, joined by '+'."
  Its criterion: check-boot-line compares that line, on every boot, with
  the name the profile's tiers compose; its control changes Eevdf's `NAME`
  and reads `FIRED`.
The real-time ids S13 set aside start at H.SCHED.19; `mode_available`'s
owed requirement (line 21, ruling 2) takes the next free L.sched id.

**check-boot-line** (G9) takes the expected classes name from the
`class_crates` record and the profile's tier values, with a self-test for
a stack of two tiers.

## 9c. `max+`: a cyclic executive inside the windows (OK IF E1-E13, 2026-10-27)

The customer (2026-10-27), after C6: "Can you also add the more
conservative alternative to Max, so that you could technically build a
EAL 7 / SIL 4 OS with Max+". C6 gave `max` fixed priorities inside each
window. The more conservative scheduling, which some DAL A and railway
SIL 4 systems use, makes no run-time choice at all: a time-triggered table
(a cyclic executive) inside each window, each slot naming the one task
that runs in it: a cyclic executive inside ARINC windows, not APEX's
priority scheduling, and never to be read as that (ledger line 24, C1).
Reviewed before code: ledger line 24, OK IF E1-E13.

**What "technically build" means here, and what it does not.** A profile
selects code: `max+` would compile the kernel with the most conservative
choice of every flag, so that what an EAL 7 or SIL 4 evaluation needs to
reason about is as small and as static as Ferrix can make it. It does not
supply that evaluation's evidence. EAL 7 asks for a formal model of the
security policy and formally verified design and implementation
correspondence (ADV_SPM.1, ADV_FSP.6, ADV_TDS.6 or higher, ADV_IMP.2 with
formal correspondence); Ferrix has no formal proof today, and the level
says so rather than claiming it.

**Proposed:**
* **P1, a value `sched_windows = "arinc_table"`** in its own crate,
  `ferrix-sched-windows-table`: the same static major frame of windows as
  `arinc`, and inside each window a static slot table built at boot from
  the partition configuration and never changed (no priorities, no
  queues ordered at run time, no admission at run time: a task is in a
  slot or it does not run). Its own crate, so a `max+` kernel links no
  priority code and S1's evidence shows it. `lands` M3 with `arinc`,
  `built = "no"`. Allows every level that `arinc` does and `sc4_plus`.
* **P2, a level `sc4_plus`**: `sc4`'s targets with Common Criteria EAL 7
  (IEC 61508-3 SC 4, ISO 26262 ASIL D, EN 50716 SIL 4, DO-178C DAL A,
  IEC 62304 Class C, EAL 7). It covers `sc4` standard by standard. A value
  allows it only where it says so explicitly (DF1): the proposal is the
  values `max` uses that leave no run-time flexibility (`personality =
  user`, `fs = server`, `mitigations = on`, `partition_speculation = exit`,
  `ipc_fastpath = off`, `restricted_fastpath = off`, `kernel_memory =
  retyped`, `sched_dispatch = fixed`, `sched_windows = arinc_table`, the
  other tiers `absent`, `checks = on`, `program_barrier = switch`, `ssbd =
  always`, `smt = off`, `runtime_trades = none`, `runtime_settings =
  fixed`), and not `sched_windows = arinc`.
* **P3, a profile `max_plus`** (profile names are lower case, digits and
  `_`; "Max+" in prose): `max`'s values with `sched_windows =
  "arinc_table"`, level `sc4_plus`, `modes = ["HardRt"]`, buildable from
  `max`'s stage, its claim a target ("EAL 7 / SIL 4 / ASIL D / DAL A, no
  formal evidence yet") rather than a held claim.
* **P4, the audit record and the flag word**: one value more for
  `sched_windows` (three of its eight), one profile and one level more
  (six and five of sixteen), the flag word 54 of 64.

**The rule for `max+`** (the customer, 2026-10-27: "max+ shall be the
most bare microkernel possible"): every flag of `max_plus` takes the value
that links the least code and leaves the least to decide at run time,
whatever that costs in features. A flag added later declares which of its
values that is, and `max_plus` takes it; a value that adds code or a
run-time choice never allows `sc4_plus`. Where `max` and `max_plus` differ
today: the windows' slot table instead of priorities (P1) and no Linux
personality (C7). The design review is asked to name any other value of
`max` that is not the barest, and a check that holds the rule.

**Open for the customer and the consultant:**
* **C7, decided by the customer (2026-10-27): no Linux personality in
  `max+`** ("go for it", to the choice between keeping it and making
  `max+` the profile that needs it off). `max_plus` has `linux_abi =
  "off"`, the smallest kernel Ferrix can build. That value is build-only
  and carries no claim today (LINUX-ABI-SEPARATION.md §11.11): making it
  claimable, re-homing the checks that run a Linux program, is owed before
  `max_plus` can be more than a target, and `linux_abi = "off"` would have
  to allow `sc4_plus`.

**As the review has it (ledger line 24):**
* **E1, a target.** `sc4_plus` lists sc4's targets with EAL7, its `why`
  says no formal model, no formal correspondence and no evaluation stand
  behind it, `max_plus`'s claim starts "target", CLAIM.md holds neither,
  and check-build-flags refuses an `sc4_plus` profile claiming more.
* **E2.** A value allows `sc4_plus` only if it allows `sc4`: the flag
  file's coverage rule (check-item-boundary) and check-build-flags both
  hold it; `sc4_plus` adds EAL 7 alone.
* **E3, C7's consequences.** `linux_abi = "off"` and `fs = "none"` both
  require `personality = "none"`, so `max_plus` has all three, and
  `partition_speculation` and `restricted_fastpath` are `not_applicable`.
  The three allow `sc4_plus` only with their own evidence (the later steps
  of LINUX-ABI-SEPARATION §11.11, the checks that run a Linux program
  re-homed, ledger 3257 (1), 3208 (6)(g)); until then `max_plus` lists them
  under `pending`, a key a target-only profile alone may carry, and
  check-item-boundary reports it not claimable.
* **E4, the table's own crate.** `ferrix-sched-windows-table` neither
  depends on nor names `ferrix-sched-windows`; the major frame both share
  is a third crate that implements no `SchedClass`, classified before its
  code; `class_crates` gets the table crate's entry, name and marker; the
  class-crate half shows a `max_plus` kernel holds no
  `ferrix_sched_windows` symbol and a `max` kernel none of the table's.
* **E5, the table, designed before its code, each point failing closed:**
  the table is compiled in or carried in the signed image, never written
  at run time; the boot refuses a table that does not fit the major frame,
  overlaps or names no task; a task that overruns its slot is preempted at
  the slot's end and never delays the next slot; an empty slot, or one
  whose task is blocked, runs the idle task; on SMP one table a processor,
  with no migration; and where the kernel's own tasks run with no fair
  tier is answered once, for `max` and `max_plus` alike (their own slots
  or a kernel window), at the windows crates' design review.
* **E6.** HardRt holds through the windows tier for `max_plus` too; the
  mode-map check passes it with `modes = ["HardRt"]`.
* **E7.** `arinc_table` is `built = "no"`, its arm a `compile_error!`
  naming it, landing at M3.
* **E8, the counts** as check-build-flags and the files compute them: six
  profiles and five levels (of sixteen each), `sched_windows` three values
  (of eight), the flag word 54 of 64.
* **E9.** No ids for this step; the table's code reserves its own (from
  H.SCHED.19); a level and a profile are configuration and take none.
* **E11, the countermeasure floor.** The barest value is chosen among the
  values that allow `sc4`; a value whose code is a countermeasure or its
  check stays (`mitigations = on`, `checks = on` with `ferrix.checks` pinned
  to `run`, `ssbd = always`, `program_barrier = switch`, `smt = off`,
  `ferrix.vtd` and `ferrix.amdvi` on). `ipc_fastpath = off` is `max_plus`'s
  for good: the 2026-10-02 rule that may turn it on for `max` does not
  reach it. The three E3 values are the exception while pending: they
  allow neither `sc4` nor `sc4_plus` yet.
* **E12.** `slot_signing = "required"` (off lets the loader boot an
  unsigned slot, a run-time choice), with `slot_families` empty until the
  board is chosen, so no slot boots before then; `fs = none`; the
  restricted-exit flags `not_applicable`; and none of `ferrix.root`,
  `ferrix.init`, `ferrix.onexit` or `console`, which take a free value in
  `max`, is permitted: the image fixes them.
* **E13, the check.** Every flag and image key names its `barest` value
  with the reason; check-build-flags refuses one that names none, a
  barest value that does not allow `sc4` (unless pending, above), a value
  other than the barest allowing `sc4_plus`, an `sc4_plus` profile whose
  values are not the barest or whose barest value neither allows
  `sc4_plus` nor is pending, and a boot option it permits with other than
  one literal value; each with a self-test. Which value is barest stays a
  reviewed judgement, recorded in the flag file and the ledger.

## 9d. Step 4a: a stack of two tiers (OK IF F1-F16, 2026-10-27)

Step 4 brings the first class above fair, `ferrix-sched-fifo` (4b). Before
it can, the core must be able to hold a stack of two tiers. Today it
cannot: `StackQueue` is EEVDF's `RunQueue` itself, and fifteen calls in
`sched/queue.rs` and `sched/mod.rs` reach it directly (`stack.rs` lists
them). Step 4a changes no behaviour: with the fair-only stack every call
compiles to what it is today. It makes the two-tier stack compile, and
says for each call what it means once a task of a higher tier can be
queued or running.

**The fair tier, reached through the stack.** A trait of the fair crate,
`FairTier<T>`, gives the fair tier's queue inside a stack's queue:

```rust
pub trait FairTier<T>: SchedClass<T> {
    fn fair(queue: &Self::Queue) -> &RunQueue<T>;
    fn fair_mut(queue: &mut Self::Queue) -> &mut RunQueue<T>;
}
impl<T> FairTier<T> for Eevdf { /* the queue itself */ }
impl<T, U: SchedClass<T>, L: FairTier<T>> FairTier<T> for Over<U, L> { /* queue.lower's */ }
```

Fair is always the innermost tier (§6: windows, deadline, realtime,
fair), so the lower side is enough. A stack without a fair tier (`max`,
`max_plus`: C3) does not implement it, so a core that still calls the fair
tier does not compile for them: §11's N7 table, the list of what must go
through `Stack` before `max` builds, becomes compiler-checked. For the
fair-only stack `fair` is the identity and inlines away (S5: the release
ELF table again, before and after).

**Each call, under a stack with a tier above fair:**

| Call | Where | With a higher tier |
|---|---|---|
| `set_weight` | `reweight`, `set_weight` | the fair tier's alone: a weight is a fair parameter; a task of a higher tier keeps its weight for when it returns to fair |
| `for_each`, `avg_vruntime`, `current_id` | `note_pick`, `measure`, /proc | the fair tier's picture: the fairness checks and the EEVDF trace are about fair tasks; a higher tier's tasks are listed by its own class (4b) |
| `queued` | `waiting` (the timer's decision) | **the whole stack's**: becomes `Stack::len` less the running one, a trait operation; a FIFO task waiting must keep the tick |
| `set_slice_ns`, `config` | `rescale_slice`, `slice_ns` | the fair tier's, scaled by the fair tier's own length, not the stack's |
| `should_preempt` | the tick | **the stack's**: a provided-free required operation `SchedClass::should_preempt(queue)`; `Over` answers yes when the upper tier has work and the lower runs, else asks the running tier (S11: required, no default) |
| `load_weight` | `account`'s load average | the stack's: a higher tier's tasks count as load; a trait operation, its weight for a fixed-priority task the fair `NICE_0_WEIGHT` (stated, reviewed in 4b) |
| `latest_where` | `steal_candidate` | the fair tier's: only fair tasks are stolen (`steals_work`, S11); a SoftRt domain does not steal |
| `yield_curr` | `yield_now` | the running tier's: a trait operation `SchedClass::yield_current` (FIFO: to the tail of its priority, 4b) |
| `level` | the fairness check's window | the fair tier's |
| `pick` | the boot task's adoption | already `Stack::pick` |

So four calls become trait operations, required with no default (S11):
`waiting`, `should_preempt`, `load_weight`, `yield_current`; the rest go
through `FairTier::fair`. Each new operation gets its forwarding in `Eevdf`
and `Over`, a host test on a two-tier toy stack in `ferrix-sched-class`
(the toy FIFO over a toy fair class there), and the field-for-field test
on the fair-only stack (N4).

**The task's entity, S4's full form.** N2's const requires the stack's
entity to be `FairEntity`; a two-tier stack's is `Tier<U::Entity,
FairEntity>`. The task keeps the fair part as today (the record's atomics,
the run slot behind L.sched.63's cell) and gains one field for the parts
above it: `upper: UpperCell`, the higher tiers' entity and which tier the
task is in, behind a cell of its own that only the queue's lock holder
takes, as the run slot is. `Task::entity_with` assembles `Tier::Lower`
from the fair parts when the task is fair and `Tier::Upper` from the cell
when it is not; `keep_entity` takes either apart. For the fair-only stack
the cell's type is zero-sized and the const becomes "the fair-only stack's
entity is `FairEntity`, and every stack's lower-most entity is", so N2's
guarantee is kept for the path the IPC round trip takes. The policy a task
asked for (4b: `sched_setscheduler`) is what moves it between tiers, under
its queue's lock, by `remove` from one tier and `enqueue` into the other.

**Rule 7.** L.sched.64-66 (the direct switch's job fold) hold as written
while both tasks are fair. With a higher tier they are restated: a
hand-over between tiers goes through `Over::hand_over`, which falls back
to `enqueue` + `pick` when the arriving and leaving tasks are in different
tiers, and the fold applies only when both are fair. Ids for the
restatement, and for the four new operations' requirements, are reserved
from L.sched.103 before 4a's code (S13).

**The field's name** (C1 of line 13): `CpuQueue::fair` becomes
`CpuQueue::stack`, in its own commit, path-only.

**Questions for the review:**
1. Is `FairTier` on the innermost tier the right shape, or should the
   EEVDF-only calls become trait operations every class answers?
2. `queued`/`should_preempt`/`load_weight`/`yield_current` as required
   operations: accepted as S11 asks?
3. The task's `upper` cell beside the fair parts, rather than one cell for
   the whole stack entity: the fair parts stay where L.sched.63 and the
   IPC path want them.
4. A fixed-priority task's load weight as `NICE_0_WEIGHT`, or its own?
5. Which ids 4a takes.

**As the review has it** (ledger lines 26 and 27, OK IF F1-F16; the
answers to the questions above: FairTier accepted on the innermost tier,
narrowed; the operations required, with no default; the upper cell
accepted; a fixed-priority task's load weight a constant of its own; the
ids below):

* **F1, FairTier gives no `&mut RunQueue`.** `FairTier::fair(&Queue) ->
  &RunQueue<T>` for reading, and two named forwarders for the only fair
  changes the core makes, `set_weight` and `set_slice_ns`. Every change of
  membership or of the running entity goes through `Stack`, so `Over`'s
  `running_upper` cannot be broken behind its back; a check refuses
  `fair_mut` or a `&mut RunQueue` through FairTier in the core, with a
  control.
* **F2, F16, every call mapped**, seventeen (the fifteen above and
  `arm_timer`'s two), each by core function and file; the table replaces
  `stack.rs`'s list and no call reaches `RunQueue` but through FairTier or
  `Stack`:

| # | Call | Core function, file | Through |
|---|---|---|---|
| 1-3 | `current_id`, `avg_vruntime`, `for_each` | `note_pick`, `sched/queue.rs` | FairTier::fair |
| 4 | `set_weight` | `reweight`, `sched/queue.rs` | FairTier::set_weight |
| 5, 6 | `for_each` | `measure`, `sched/queue.rs` (two) | FairTier::fair |
| 7 | `set_weight` | `set_weight`, `sched/queue.rs` | FairTier::set_weight |
| 8 | `load_weight` | `account`'s load, `sched/queue.rs` | Stack::load_weight |
| 9 | `decision_in_ns` | `arm_timer`, `sched/queue.rs` | Stack::decision_in_ns |
| 10 | `current` | `arm_timer`'s cpu.max cut, `sched/queue.rs` | Stack::current |
| 11 | `latest_where` | `steal_candidate`, `sched/queue.rs` | FairTier::fair |
| 12 | `set_slice_ns` | `rescale_slice`, `sched/queue.rs` | FairTier::set_slice_ns |
| 13 | `config` | `slice_ns`, `sched/queue.rs` | FairTier::fair |
| 14 | `should_preempt` | `should_preempt`, `sched/queue.rs` | Stack::should_preempt |
| 15 | `queued` | `waiting`, `sched/queue.rs` | derived (F3) |
| 16 | `yield_curr` | `yield_now`, `sched/mod.rs` | Stack::yield_current |
| 17 | `level` | `open_window`, `sched/mod.rs` | FairTier::fair (levelling is a read-and-reset of fair's lag, a forwarder `FairTier::level`) |

* **F3, `waiting` derived**: `Stack::len` less whether something runs, in
  one core function; the timer reads `decision_in_ns` (F14), and
  `waiting` stays for `/proc` and the balancer's snapshot.
* **F4, preemption consistent.** On the tick `should_preempt` and `charge`
  cannot disagree (a host test on the toy stack holds them together);
  preemption on a wake, local and remote, goes through
  `Stack::should_preempt` too, so a woken FIFO task does not wait for a
  tick: designed here, its requirement with 4b.
* **F5, weights never lost.** A weight, nice or `cpu.weight` change on a
  task in a higher tier goes into its kept fair record and applies when it
  returns to fair; never dropped, never applied to a queue that does not
  hold it; a check shows it.
* **F6, the upper cell**: zero-sized for the fair-only stack, asserted at
  compile time; taken only by the queue's lock holder, any new `unsafe`
  traced; the task's tier is kept in the cell and read without the lock
  only as a hint (by `/proc`), never to decide.
* **F7, N2's const checkable**: FairTier carries `type Entity =
  FairEntity<T>`; a stack's entity nests down to it; the fair-only
  `StackEntity` is exactly `FairEntity`. S5: the release ELF table before
  and after, and the round trip on the landing hash.
* **F8, the other N7 rows for a higher-tier task.** The sleepers'
  `Timeline` and the dead's are data structures keyed by time and
  arrival, not fair's policy: a task of any tier sleeps and is filed dead
  in them as today (each carries its own sleep slot and run slot). A
  steal never takes a higher-tier task (`steal_candidate` reads the fair
  tier alone). The job fold and the carried weights are fair's and apply
  only between fair tasks (F10). None needs `Stack` in 4a; 4b restates
  them if FIFO needs more.
* **F9, inheritance designed now, built in 4b.** A child made by `fork`
  or `clone` inherits its parent's tier and policy, unless the parent set
  `SCHED_RESET_ON_FORK`, when it starts fair at nice 0 (nice raised to 0 if
  negative), as Linux does; `launch::load_native` makes its process by the
  same function, so a native launch inherits exactly as a fork does.
* **F10, rule 7 in the core.** The job fold in `sched/direct.rs` is gated
  in the core on both tasks being fair, not left to `Over`'s fallback;
  L.sched.64-66 are restated in place, keeping their ids; a host test
  shows a cross-tier hand-over does not fold, and a boot check where one
  can reach it.
* **F11, the two-tier stack compiles, shown by a build**: the first
  two-tier stack the core is built with is 4b's `desktop` with
  `sched_realtime = "fifo_rr"`; until then the trait side is shown by the
  toy FIFO-over-toy-fair host tests in `ferrix-sched-class` and a compile
  test there instantiating `Over<Toy, Eevdf>` against every operation the
  core calls (proposed; for the consultant to accept).
* **F12, ids**, reserved by a docs landing before code, each unit a core
  function: L.sched.103 the timer armed for the stack's `decision_in_ns`
  (`CpuQueue::arm_timer`); L.sched.104 tick preemption through
  `Stack::should_preempt`; L.sched.105 the processor's load through
  `Stack::load_weight`; L.sched.106 yield through `Stack::yield_current`;
  L.sched.107 the upper cell taken only under its queue's lock;
  L.sched.108 a weight kept for a task outside the fair tier.
  L.sched.64-66 restated in place. Wake preemption (F4), inheritance (F9)
  and the cpu.max term (F15) take theirs with 4b.
* **F13**: the rename landed as 62b1607b5, path-only (its four boots'
  identical lines owed on the gate host); where it stands names lines 26
  and 27.
* **F14, `Over::decision_in_ns`**: the upper tier's answer while it runs
  (lower tasks waiting cannot displace it); `Some(0)` while the lower runs
  and the upper has work; else the lower's. A host test: `None` only when
  no decision can be due, never later than `remaining_ns` when both are
  `Some`, agreeing with `charge`. In 4b FIFO answers `None` and RR its
  quantum while a task of its priority waits.
* **F15, the cpu.max cut, decided in 4b** (recommended yes):
  `account_in` charges the running task's group whatever its tier, and
  `arm_timer`'s comment is rewritten so; cpu.max bounds a FIFO task as a
  safety net, as Linux's `rt_runtime` does; HardRt tasks are governed by
  their windows or budgets, not cpu.max. Its own requirement and control
  in 4b.
* **The load unit** (answer 4): `LOAD_UNIT` in `ferrix-sched-class`, the
  fair crate asserting at compile time that it equals `NICE_0_WEIGHT`, so a
  class crate needs no other class crate; what it does to the load
  average and balancing is stated and reviewed in 4b (C2: the `/proc`
  load figures change meaning once a higher tier exists, documented
  then).

## 10. The review's conditions (OK IF S1-S14, 2026-10-26)

A certification-consultant subagent of the customer's session, ledger line
11 of the ledger it keeps in this container (to be copied to the gate
host's). Each is a condition on the step named; S1-S12 and S14 are written
into this design before step 2's code.

* **S1, absent code, evidence.** `cargo tree` and Cargo metadata are not
  evidence (Cargo lists every class crate in every build). What is: the
  binary's link line (`--print link-args` or the linker map, kept per
  profile and architecture by `test-removable`) names no rlib of an absent
  crate; `check-removable-symbols` finds none of its symbols in the ELF; a
  present build shows a marker inlining cannot remove (the boot line's
  stack names, and `check()` kept out of line); and a planted unconditional
  use of the crate, run as a control, reads `FIRED`. *Restated by line 22
  (G3):* the release link is fat LTO with one codegen unit, so no rlib
  reaches its link line; "the link line" is that of a link with the same
  architecture, profile and flags and LTO alone turned off, whose map
  shows no member of an absent crate's rlib, and the ELF half is read from
  the shipped release ELF (§9b).
* **S2, the rule-6e exception**, as §5.3: one file, hand-written arms, an
  `Empty` class for an absent tier, the arms held one to one with the flag
  file, nothing else reading a scheduler flag.
* **S3, a record kind for the class crates**, with self-tests and
  controls; §6a no longer claims "no new check code".
* **S4, per-task state in `Task`**, as §5.4.
* **S5, the direct hand-over is a required operation.** The fused hand-over
  the IPC round trip rests on (today `RunQueue::hand_over`, L.sched.55 and
  64-66, with the job fold) is a required trait method, not a provided
  default. Step 2 lands only if the round trip, measured before and after
  under the `optimize-ipc-round-trip` protocol on x86-64 and both Arm
  boots, shows no regression beyond noise.
* **S6, an inventory before step 2**: every `ferrix-sched` item and every
  scheduler unit in core `sched/**` and `object/quota.rs` (the `NICE_`
  constants, about 53 uses, `carried_weight`, the job fold, `wake_wide`,
  `Timeline`, `CpuSet`, `domain.rs`) assigned to the interface crate, a
  class crate or the core; each L.sched requirement re-pointed with its
  flag predicate; moved bodies byte-identical, coverage carried by span
  (F-75).
* **S7, the core ring only when its gates pass**: `forbid(unsafe_code)`,
  the fallible-allocation gate (so `new_queue` returns a `Result`), the
  overflow lint, coverage and traceability read the class crates and pass,
  else this repeats F-56. `ferrix-sched`'s two `unclassified_deps` entries
  go when it moves and F-76's count is recounted in FINDINGS.md and
  README.md (F-76 stays open for the other libraries). Each value's allowed
  levels match its crate's level; new values start at `none`.
* **S8, the audit words.** One flag a tier plus `sched_dispatch`, less
  `schedule`, makes twenty flags against `audit_words`' sixteen two-bit
  slots, and `sched_fair` will want more than four values: the start-up
  audit record (AUDIT.md §1, FAU_GEN.1 b) is redesigned in step 3's review.
* **S9, S10, switching stays out of certified profiles**, as §5a.
* **S11, defaults.** A source check that no claimed profile inherits a
  provided default, with a planted control; `Over`'s work-stealing answer
  is a function of the queues, not the constant `STEALS_WORK`.
* **S12, modes as a profile key.** The modes a profile allows become a key
  the kernel enforces; the two-sided rule keeps its two controls; each
  allowed mode is exercised by that profile's rows.
* **S13, requirement ids.** L.sched.96-101 and H.SCHED.17 are taken; new
  ids start at L.sched.102 and H.SCHED.18. Step 1 (this design): none.
  Step 2: none if behaviour is unchanged and only units are restated; any
  new boot check or refusal takes from L.sched.102. Step 3: ids only if
  `Mode`'s refusal is restated. Each class (step 4) and the switch (step 5)
  reserves at its own design review: H.SCHED.19 on (G7 took H.SCHED.18) for SoftRt's starvation
  bound and HardRt's admission (ASR-8), L.sched ids for the core's
  dispatch, and a prefix of its own for each class crate.
* **S14, where it stands**: below, and REALTIME.md §4 points here.

Lesser points: C1 (Linux numbers in the personality's table, §6a); C2,
`boost` as a per-class `Entity` operation cannot move a task between tiers
(a fair owner boosted by a FIFO waiter changes tier), so the owned wait
queue's review defines it at the stack level; C3 and C4 (§7a); C5, the
switch's bound goes into MEMORY-AND-TIMING when step 5 lands.

## 11. Step 2's inventory (S6)

Every item of `ferrix-sched` (`src/lib/kernel/sched`, 4,876 lines with its
tests) and every scheduler unit of the core, assigned before the move. A
moved body is byte-identical; only its path changes.

**To `ferrix-sched-class`, the interface crate** (no class's policy, shared
by every class and by the core):

| Item | From | Why here |
|---|---|---|
| `CpuSet`, `MAX_CPUS` | `domain.rs` | affinity and domain membership, named by the whole kernel (about 30 files) |
| `Mode`, `Class`, `Domain`, `check_partition`, the domain errors of `SchedError` (`ModeUnavailable`, `EmptyDomain`, `Overlap`, `Uncovered`, `NoSuchCpu`) | `domain.rs`, `lib.rs` | modes name tiers, not crates (§6) |
| `Load`, `CpuLoad`, `LOAD_SCALE`, `LOAD_PERIOD_NS`, `place`, `Placement`, `flip`, `FLIP_DECAY_NS`, `wake_wide`, `pull_fits`, `utilisation`, `BALANCE_THRESHOLD`, `imbalance`, `busiest`, `Balance`, `quietest` | `balance.rs` | load tracking and placement, which the core does for any class that steals work |
| `SchedClass`, `Over`, `Empty` | new | §4 |

**To `ferrix-sched-fair`, EEVDF**:

| Item | From |
|---|---|
| `RunQueue`, `Config`, `EntityState`, `EntityView`, `Refused`, `Left`, the queue errors of `SchedError` (`ZeroWeight`, `Duplicate`, …), the private `Entity`, `Key` and the arithmetic | `lib.rs` |
| `NICE_0_WEIGHT`, `weight_of_nice` and its table | `lib.rs` |
| `Node`, `Slot`, `Timeline` (the tree and the core's sleepers): found at the split to be built on the fair entity (`Node` holds an `Entity<T>`), so they stay here; a `max` without fair needs a sleeper queue of its own | `tree.rs`, `timeline.rs` |
| `carried_weight`, `carried_weight_with`, `JobPlan`, `plan_job_fold`, `settle_job_fold` (the job weights and the direct switch's fold) | `lib.rs` |
| `slice_for` | `balance.rs` (the slice is EEVDF's) |
| the property tests of the queue, the weights and the fold | `tests.rs` |
| `impl SchedClass for Eevdf` | new, forwarding to `RunQueue` |

`SchedError` is split by the line above; the two halves keep their
variants' names and messages.

**Stays in the core** (`src/kernel/src/sched/`): `queue.rs` (`CpuQueue`,
whose `fair: RunQueue` becomes the stack's queue in step 2's second half),
`task.rs` (whose `EntityState`, `Node` and `Slot` fields become the stack's
`Entity`, S4), `direct.rs` (the direct switch, through the required
hand-over, S5), `mod.rs`, `preempt.rs`, `wait.rs`, `work.rs`, `borrow.rs`,
`inspect.rs`, `trip.rs`, `vtime.rs` and their checks. `object/quota.rs`
keeps `cpu.weight` and the job weights; it names `NICE_0_WEIGHT` from the
fair crate, which is one of the `NICE_` users that must be absent in
`max`, which has no fair tier (C3, §7a).

**Users of the moved names**, re-pointed by path only: the uses of
`NICE_0_WEIGHT` and `weight_of_nice` the table below counts (attributes,
the stage checks, quota, spawns), 30 of `CpuSet` and `MAX_CPUS` (mm, arch, user, object), and the
core scheduler's own imports in `sched/{mod,queue,task,direct,check}.rs`.

**What the core keeps of the fair crate outside `Stack`** (the split's
review, N7, ledger line 19; counted at 1c725245 by `git grep` over
`src/kernel`, a use being one line naming it). Each
fixes the fair class in every profile until it goes through `Stack`; since
`max` has no fair tier (C3, §7a), every row must before `max` can build:

| Use | Where | Why outside `Stack` today |
|---|---|---|
| `NICE_0_WEIGHT` (148 lines in 54 files: 42 in 20 product files, 106 in check files) and `weight_of_nice` (8 lines, 2 of them comments) | spawns of kernel tasks, in the load ring as well as the core (`interfaces/*`, `fs/root_disk.rs`, `fs/terminal.rs`, `fs/timerfd.rs`, `net/mod.rs`, `discovery/devmgr.rs`, the gs201 watchdog, x86-64's `msrlog.rs` and `powerctl.rs`), `syscall/attributes.rs` (nice), `syscall/kill.rs`, `object/quota.rs` (`cpu.weight`), the boot checks (`sched/check.rs`, `object/fast_path_check.rs` 1709 and 1752) | a task's weight is the fair class's parameter; in a stack it becomes the task's `Params`. A `max` without fair (C3) changes the load ring's spawns too |
| `carried_weight`, `carried_weight_with`, `JobPlan`, `plan_job_fold`, `settle_job_fold` | `object/quota.rs`, `sched/direct.rs`, `sched/queue.rs` | the job weights and the direct switch's job fold (L.sched.64-66), which hold as written only while both tasks are fair (N4): the step that first puts a tier above fair owes their restatement under rule 7, with its check |
| `slice_for`, `Config` | `sched/queue.rs` | the fair queue's slice, rescaled by how many are runnable |
| `Timeline`, `Node`, `Slot`, `EntityState` | `sched/queue.rs` (the sleepers), `sched/task.rs` (the run and sleep slots) | the sleepers' timeline is built on the fair entity; the task's run slot and record are the stack's `Entity` through `Task::entity_with` (N2) |
| `Timeline` of the dead | `sched/mod.rs` 519 and 528 (`Zombies`) | the dead are filed in their run slot on a timeline of their own, not the sleepers' |
| `Option<EntityState>` | `sched/mod.rs` 2303 (`move_onto`, after `CpuQueue::release`) | a steal carries the record its victim's queue let go to the queue it moves to |
| `FairEntity` | `sched/stack.rs` | the const requiring the stack's entity to be the fair one (N2) |
| `Eevdf` | `sched/stack.rs` | the stack itself |

**Requirements.** Every L.sched unit names a core function
(`sched::queue::CpuQueue::hand_over`, `::arm_timer`, `::sleeper_due`, …),
none a library item, so no unit was re-pointed by the move. Two statements
named the library in prose (L.object.160's `ferrix_sched::carried_weight`,
and the criteria of L.sched.55 and its high-level parent naming "the
ferrix-sched host test"); the split restated them as `ferrix-sched-fair`
(b829273f, 8062be55; ledger line 12). `certification-item.json`'s
`unclassified_deps` entries for `ferrix-sched` became entries for the two
new crates (the core both, the item the fair one) until S7's gates pass,
and F-76 was recounted (26 libraries, 22 core, 16 item).

## Where it stands

2026-10-26: designed and reviewed, OK IF S1-S14; the conditions are
written in above. LAVD is desktop only (the customer: it "does not have to
be included in max"). Step 2 started: the inventory (§11, S6) written;
the interface crate `ferrix-sched-class` built alone
(`src/lib/kernel/sched-class`: `SchedClass<T>` with the required
`hand_over`, `put_back` for a stack's lower tier, `steals_work` as a
function of the queue; `Over`, `Tier`, `Empty`; seven host tests over toy
classes), named by no kernel crate yet.
`ferrix-sched` split (§11): `ferrix-sched-class` holds the domains, the
load tracking and balancing and `DomainError`; `ferrix-sched-fair` holds
EEVDF, the tree, the timeline, the weights, the fold and `slice_for`, and
implements `SchedClass` as `Eevdf` (one host test holds the interface to
the queue). The kernel's 70 files re-pointed by path only; booted on
x86-64, AArch64 and ARMv7-A. Its code review: OK IF 1-10, ledger line 12
of the container's ledger (to be copied to the gate host's); 1-3, 5, 6, 9
and 10 met (8062be55), 4 (coverage after the final rebase), 7 (gate rows
on the landing hash) and 8 (S5's evidence) owed: the release ELF's IPC-path functions are not identical
(below).

**The core through `Stack` (2026-10-26, 7fd2b9ea; its code review OK IF
1-6, ledger line 13 of the container's ledger, to be copied to the gate
host's).** `src/kernel/src/sched/stack.rs` names `Stack = Eevdf` once; the
core's queue is `StackQueue`, and the operations the trait has --
enqueue, remove, remove the running one, pick, current, the hand-over,
charge, the remaining slice, the length, the check -- go through
`Stack::` (with `current`, `len` and the emptiness test routed too, line
13's condition 4); the EEVDF-only calls `stack.rs` lists stay on the
queue. The
fair queue's `remove`, `remove_curr`, `detach` and `hand_over` now answer
the entity as one value, `(EntityState, Slot)`, the trait's shape, so the
adapter is a pass-through. No flag reads it yet. **The interim, as line 13 accepts it** (N1): (a)
`src/kernel/src/sched/stack.rs` reads no cfg and is the only file naming
the class type; (b) no `sched_*` flag or value enters `build-flags.toml`
or a profile while `Stack` is in the core -- the selection crate as S2
describes it lands with or before the first one, with check-build-flags'
arm controls and the check that nothing else reads a scheduler flag; (c)
this paragraph records it.

**What the release ELF says (x86-64, `--release`, symbols with the crate
names normalised).**

| Function | before the split (62388fd0) | the split (b829273f) | the core through `Stack` |
|---|---|---|---|
| `RunQueue::enqueue` | 0x4b4 | 0x4c0 | (inlined as before) |
| `RunQueue::set_weight` | 0x325 | 0x332 | 0x332 |
| `sched::choose_next` | 0x58c | 0x58c | 0x58c |
| `ferrix_fast_write_read` | 0x27ac | 0x27a9 | 0x27d4 |

The first cut, with the fair queue's own tuple shapes, made the fast path
0x281a and `choose_next` 0x5ae, mostly stack moves re-packing
`(EntityState, Slot)`; answering the entity as one value brought
`choose_next` back to the same size and the fast path to 43 bytes over
the split's, about ten instructions, mostly the same moves in another
order. Two other cuts were measured and dropped: a refusal carrying the
state (0x2858) and one carrying the whole entity (0x2807). So S5 is still
owed: the round trip measured before and after under the
`optimize-ipc-round-trip` protocol on the gate host, or the fast path's
remaining difference removed.

**N2, the entity in `Task`** (ledger line 14, OK IF 1-4): what a task is
queued with is `StackEntity`, `<Stack as SchedClass<Arc<Task>>>::Entity`,
assembled by `Task::entity_with` from the record's atomics and the run
slot behind L.sched.63's cell, and kept by `Task::keep_entity` and
`Task::keep_refused`. A const in `stack.rs` requires it to be
`FairEntity<Arc<Task>>`; it may go only with the change S4's full form
makes when a stack's entity first differs (step 4), and the commit adding
a second class says so. Owed with line 13's: `sched/queue.rs:747`, the
hand-over's entity line, measured on the gate host after the final
rebase; S5's round trip and the four boots taken on a hash that includes
it.

**N3, the marker** (S1): `Over`'s `NAME` is its classes' names, top tier
first, joined by `+`, built at compile time; `Empty` names nothing. Each
boot prints it after the scheduler starts (`sched    classes eevdf` on
the fair-only stack), and every class's `check` and `Over`'s are
`#[inline(never)]`, so a present class shows in the release ELF as its
check (`<Eevdf as SchedClass<Arc<Task>>>::check`, a local text symbol in
the x86_64, aarch64 and armv7a release kernels by `llvm-nm -C`). Reviewed,
ledger line 15, OK IF 1-4: the marker in the Arm release kernels (met),
the line named at landing, and with line 13's 1 and 3 the boots as gate
rows showing the classes line and S5's measurement on a hash including
it. The line is informational until step 3 gives the stack a flag to be
compared with, when it takes a requirement reserved from L.sched.102; the
selection crate then also asserts at compile time that the name is not
empty.

**N7** reviewed, ledger line 19, OK IF 1-4, met in f9165ad1.

**Step 3's design** reviewed, ledger line 20, OK IF D1-D13 (D13: this
line), written in as §9a.

**Step 3a, built** (for the consultant's code review against line 20):
* The flags (D1): `schedule` gone; `sched_dispatch`, `sched_windows`,
  `sched_deadline`, `sched_realtime` and `sched_fair` in its place with the
  allows §9a gives. **`sched_ext` is not declared**: the flag file's rule
  that a flag has two values at least (check-item-boundary) refuses a flag
  of `absent` alone, and ext has no design to give it a second; it arrives
  with that design, which also places it (C1). The audit word's tiers are
  therefore four.
* The profiles (D1, D2) as §9a's table, less `sched_ext`, each with the
  comments D2 asks; each gains `modes` (`present`, `desktop`: Throughput;
  `fast`, `max`: HardRt).
* The selection crate (D3, D4, D7): `ferrix-kernel-sched-select`, its root
  `src/kernel/src/sched_select.rs`, the arms file: a `Value` const and a
  tier alias each built value, a `compile_error!` each unbuilt one,
  `Stack = Windows<Deadline<Realtime<Fair>>>` under `fixed`, which today
  is `Eevdf` itself, the stack's name asserted not empty, and
  `mode_available`. Ring core in `certification-item.json` with its two
  `unclassified_deps` under line 20; a layering rule; built by the one
  build script; the core, the item (`build_flags.rs`) and the binary
  depend on it. `sched/stack.rs` re-exports its `Stack`; N2's const holds
  unchanged.
* The flag word (D5): `build_flag_word!` and `flag_word` moved into the
  arms file, the core re-exporting both; `flag_word` asserts at most 64
  values; the binary asserts `SELECT_FLAG_WORD`; 51 of 64, recorded in
  BUILD-FLAGS.md.
* Who reads the flags (D6): `build_flags.rs` takes each `sched_*` flag's
  value and place from the arm's const; check-build-flags refuses a
  `ferrix_sched_*` cfg anywhere in the kernel but the arms file, and holds
  the arms one to one with the flag file (one arm a value, place and
  presence right, `compile_error!` exactly for the unbuilt), with
  self-tests for a planted read, a value with no arm, a value with two, an
  unbuilt value given code, a wrong place and an undeclared value.
* The audit words (D8): the first word the fourteen other flags in bits 0
  to 27; the second the profile, the level, `sched_dispatch` in 8-9 and
  the four tiers in 10-21. AUDIT.md §1 restated; BUILD-FLAGS.md,
  MICROKERNEL.md, REALTIME.md, RUNTIME-SETTINGS.md and
  LINUX-ABI-SEPARATION.md no longer name `schedule` as a flag.
* Modes (D9): HardRt uses the deadline or the windows tier;
  `mode_available` is the selection crate's const fn the core consults
  before making its Throughput domain; check-build-flags holds each
  profile's `modes` to the modes whose tiers are present, both ways, and
  (D10) a claimed profile to `sched_dispatch = "fixed"`, with self-tests.
  `mode_available` is named by no requirement (937 named by none, from
  936), nor is `sched::init`'s refusal it guards, which every buildable
  profile folds away: no id for 3a (D12). **Owed**: a requirement from
  the next free L.sched id (L.sched.102 is the classes line's, §9b) naming
  both, with the first buildable profile that has fair
  absent or allows a mode beside Throughput (ledger line 21, ruling 2).
* The `classes` boot line moved from `main.rs`'s `start_scheduler` into
  `sched::init`'s end, the same place in the boot: printed from
  `start_scheduler` it made that function 72 lines against the complexity
  baseline's 71, a failure 94e9edf6 brought and this repairs.

**3a reviewed**: ledger line 21, OK IF 1-7; 1-5 met in 7d26af03, 7 by
its message; 6 owed on the gate host (below, with the desktop build's
`build` line and condition 1's control).

**`max+` built, reviewed**: ledger line 25, OK IF 1-6 (E13's waiver of
the three pending values confirmed as intended). Met: (1) `pending` only
at `sc4_plus` on a target-only claim, each entry naming its ledger line,
and never on a countermeasure's flag (check-build-flags), with
self-tests; (2) signed slots only with no board named is not buildable,
in check-item-boundary and xtask alike (self-tests); (4) BUILD-FLAGS.md
says xtask's rule 3 refuses `max_plus` on `personality = none` without
reading `pending`, failing closed; (6) this line; C1, the rename. (3) is
owed before `max_plus` builds (M5): today a profile's `boot_options`
records the options it permits and the kernel does not read it, so
`max_plus`'s dropped options (`ferrix.root`, `ferrix.init`,
`ferrix.onexit`, `console`) are recorded, not enforced, until the kernel
refuses or fixes at compile time every option an `sc4_plus` build does
not permit, failing closed, under a requirement reserved first and with a
control. C2: the rings and crates at level `present`, below `sc4_plus`,
are the gap `max` has at `sc4`, and close with it. (5) owed on the gate
host: the four boots, the Arm builds, the controls.

**`max+` reviewed**: ledger line 24, OK IF E1-E13, §9c. Built here (the
level, the value, the profile, the `barest` keys, `pending`, the checks):
E1-E3, E6-E8, E11-E13; owed: E4 and E5 with the table crate (M3), E10
this line and the landing's; the controls of E13 on the gate host. E13's
"the barest value allows `sc4`" is read as excusing the three E3 values
while pending, for the consultant to confirm.

**Step 3b's design** reviewed, ledger line 22, OK IF G1-G10 (G10: this
line), written in as §9b; H.SCHED.18 and L.sched.102 reserved.

D3's release ELF table, the stack the fair class alone through the
selection crate:

| Function | 94e9edf6 (before 3a) | 4287d870 (3a, its conditions) |
|---|---|---|
| `choose_next`, x86-64 | 0x58c | 0x58c |
| `ferrix_fast_write_read`, x86-64 | 0x27d4 | 0x27d4 |
| `choose_next`, AArch64 | 0x74c | 0x74c |
| `choose_next`, ARMv7-A | 0xac4 | 0xac4 |

**Step 3b, built** (for the consultant's code review against line 22):
* G1: `ferrix-sched-fixture`, `src/lib/kernel/sched-fixture`: a FIFO class
  of at most sixteen entities, `no_std`, `forbid(unsafe_code)`,
  `publish = false`, no `ferrix_` cfg, four host tests; a dependency of
  the selection crate alone, named by no file, declaring no flag value.
* G2: the arms file's crate-level `expect(unused_crate_dependencies)`,
  naming the fixture in its reason; rule (d) refuses any name of it.
* G4, G5: `class_crates` in `certification-item.json` (fair: `sched_fair`
  = `eevdf`, its name `eevdf` and its marker; the fixture), with
  `named_outside` naming the N7 rows for fair in the core, the item and
  the load (the load's spawns use the `NICE_` weights too). check-item-
  boundary holds rules (a)-(d) and that a recorded name is the class's
  `NAME`, each with a self-test; its F-76 rule passes a fixture of the
  selection crate, which no register entry stands for.
* G3, G8: `check-removable-symbols.py --class` judges a release ELF and a
  non-LTO map against the record (five self-tests); `cargo xtask
  test-removable` gains the class-crate half for `present` and `desktop`
  on each architecture, the map's link made through `FERRIX_LINK_MAP`
  (LTO off, its own target directory). KERNEL-CRATES.md §6 states the LTO
  argument; S1 is restated (§10).
* G7: H.SCHED.18 (parent ASR-8) and L.sched.102 (unit `sched::init`)
  written, their reservation released in the same commit; L.sched.102 is
  verified by xtask's `boot_line`, H.SCHED.18 by test-removable's
  class-crate half.
* G9: check-boot-line composes the expected classes line from
  `class_crates` and the profile's tier values and refuses a boot whose
  line differs, with self-tests for a stack of two tiers and an absent
  one.

**3b reviewed**: ledger line 23, OK IF 1-8; G3's deviation accepted (a
crate of generics alone has no object of its own in the non-LTO map, so
its code is read there as its functions monomorphised into another
crate's member; the ELF marker stays the positive proof). Met on the
branch: (1) the judge refuses a map that names no demangled path of the
core (empty, truncated or mangled), with a self-test, and test-removable
deletes a stale map before its non-LTO link; (2) `Joined::as_str` refuses
a non-UTF-8 name at compile time again, its panic expected with an audit
reason; (3) check-build-flags holds Stack's nesting to the flag file's
tier order, with self-tests; (4) rule (c)'s text names the load; (8) this
line. Owed: (5) the reservation of H.SCHED.18 and L.sched.102, commit
25efdb079 on this branch (4287d870 before the rebase, as 8f5f375dd's
message names it), landed docs-only on develop, the branch rebased and
traceability --check clean with TRACEABILITY.md regenerated, before
land.sh take; (6) the controls with gate.sh, each `FIRED`: G6 (i)-(iii),
L.sched.102's (Eevdf's NAME changed) and condition 1's; (7) the rows on
the landing hash: G8's class-crate half on all three architectures for
`present` and `desktop`, the four boots with check-boot-line on both
lines, coverage after the final rebase (the 7 anchors dropped in the
rebase: the sched/queue.rs, sched/direct.rs and gs201 watchdog.rs lines
of line 13's condition 2), and lines 20's and 21's owed items. The
container's x86_64 run of the class-crate half is development evidence,
not a row.

Owed on the gate host with line 20's: the controls of D6 and D10 run as
`gate.sh control` with `FIRED` (a `cfg!(ferrix_sched_fair = ..)` planted
in a core file, a declared value with no arm, a value with two arms,
`switchable` planted in `fast.toml`), the four boots as rows with the new
`build` line and check-boot-line passing, S5's round trip on the landing
hash. Next: 3b; N4's measurement on the gate host. C3 decided by the
customer on 2026-10-27: no fair tier in `max` (§7a).
