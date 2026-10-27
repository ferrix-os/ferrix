# Scheduling classes as removable crates

**Design reviewed, 2026-10-26: OK IF S1-S14** (§10; ledger line 11 of
this container's consultant ledger, still to be copied to the gate
host's). Not built. The customer asked
(2026-10-26): "design all our schedulers to be crates with one common
compile-time trait interface between the kernel, so that we can select our
desired schedulers by enabling or disabling a crate via a flag … and that
certification which requires no dead code can easily be achieved by just
moving out the crate". This is that design, for stage 14
([REALTIME.md](REALTIME.md) §3) and MICROKERNEL.md's `schedule` flag. It
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
| `ferrix-sched-windows` | core | later (M3): the static time-partition schedule of MICROKERNEL.md K9, with EDF inside each window |
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
  does. The present `schedule` flag (`fair` | `edf` | `windows`) is
  replaced by them. Today's values, with room for more (§6a):

  | Flag | Values |
  |---|---|
  | `sched_windows` | `arinc` (`ferrix-sched-windows`), `absent` |
  | `sched_deadline` | `edf_cbs` (`ferrix-sched-edf`), `absent` |
  | `sched_realtime` | `fifo_rr` (`ferrix-sched-fifo`), `absent` |
  | `sched_fair` | `eevdf` (`ferrix-sched-fair`), `absent` |
  | `sched_ext` | `absent` |

  | Profile | windows | deadline | realtime | fair | ext |
  |---|---|---|---|---|---|
  | `max` | `arinc` | `edf_cbs` | `absent` | `absent` or `eevdf` (§7) | `absent` |
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
   class in every profile, inside EDF's idle time only on `HardRt` CPUs.
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
* Open for the customer: whether fair is optional in `max` (C3: without
  it, every kernel worker and server needs a reservation, and every user of
  the `NICE_` constants must be absent too); where the fair server lives is
  free (C4: a crate of its own only once two classes link it).

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
  use of the crate, run as a control, reads `FIRED`.
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
  reserves at its own design review: H.SCHED.18 on for SoftRt's starvation
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
fair crate, which is one of the `NICE_` users C3 would make absent in a
`max` without fair.

**Users of the moved names**, re-pointed by path only: the uses of
`NICE_0_WEIGHT` and `weight_of_nice` the table below counts (attributes,
the stage checks, quota, spawns), 30 of `CpuSet` and `MAX_CPUS` (mm, arch, user, object), and the
core scheduler's own imports in `sched/{mod,queue,task,direct,check}.rs`.

**What the core keeps of the fair crate outside `Stack`** (the split's
review, N7, ledger line 19; counted at 1c725245 by `git grep` over
`src/kernel`, a use being one line naming it). Each
fixes the fair class in every profile until C3 is decided, or goes through
`Stack` when a second class needs it:

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

Next, each pushed as it is done so another session can take it up: N4's
measurement and N7's review; then the selection crate as S2 has it (N1). Still open for the customer: C3, fair in
`max`.
