# Scheduling classes as removable crates

**Design for review, 2026-10-26.** Not built. The customer asked
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
table, nothing registered at run time.

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

One interface, associated types for everything a class keeps, associated
functions with no `self` where state is passed in (the shape KERNEL-CRATES
§6.3 chose for `Policy`):

```rust
pub trait SchedClass: 'static {
    /// What the boot line and the audit record call it.
    const NAME: &'static str;
    /// Whether an idle CPU of a domain using it may take its tasks from
    /// another CPU. False for every real-time class: partitioning is what
    /// makes admission mean anything.
    const STEALS_WORK: bool;

    /// What one CPU keeps: fixed capacity, made once at boot.
    type Queue;
    /// What one task keeps while it is in this class.
    type Entity: Default;
    /// What a task asks for: a weight, a priority, a reservation.
    type Params: Copy;

    fn new_queue(cpu: CpuId, capacity: usize) -> Self::Queue;

    /// Whether `params` may join `queue` without breaking what it has
    /// already admitted. Fair answers yes; FIFO checks the priority range
    /// and the caller's right; EDF runs its utilisation or density test.
    fn admit(queue: &Self::Queue, params: &Self::Params) -> Result<(), Refused>;

    fn enqueue(queue: &mut Self::Queue, entity: &mut Self::Entity, task: TaskRef, now: Instant);
    fn dequeue(queue: &mut Self::Queue, entity: &mut Self::Entity, task: TaskRef);
    /// The task this class would run now, if it has one ready.
    fn pick(queue: &mut Self::Queue, now: Instant) -> Option<TaskRef>;

    /// Charge the running task what it used. `Throttle` takes it off the
    /// CPU until its next replenishment (EDF, the fair server).
    fn charge(queue: &mut Self::Queue, entity: &mut Self::Entity, ran: Nanos, now: Instant) -> Charged;
    /// Whether a task waking into this class preempts the one running in it.
    fn preempts(queue: &Self::Queue, waking: &Self::Entity, running: &Self::Entity) -> bool;
    /// The next instant this class needs the CPU's one-shot timer for:
    /// a slice end, a replenishment, a deadline.
    fn next_event(queue: &Self::Queue, now: Instant) -> Option<Instant>;

    /// The class's own invariants, for the boot checks and host tests.
    fn check(queue: &Self::Queue) -> Result<(), &'static str>;
}
```

A **class stack** is a type, built from the compiled classes in priority
order by the interface crate's combinator, and itself a `SchedClass`:

```rust
pub struct Over<Upper: SchedClass, Lower: SchedClass>(PhantomData<(Upper, Lower)>);
// e.g. Over<Edf, Over<Fifo, Fair>>
```

`Over::pick` asks the upper class first and the lower only if it has
nothing; its `Queue` is the pair of queues, its `Entity` an enum of the
two, `next_event` the earlier of the two, `STEALS_WORK` the lower's only
when the upper has nothing admitted. Monomorphised in the binary, the
whole stack inlines into one `pick`: no dispatch at run time, and an absent
class leaves no trace in the generated code.

Inheritance (REALTIME.md §3.4) needs one more operation, with the same
shape: `fn boost(entity: &mut Self::Entity, donor: &Donor)` and its
`unboost`, where `Donor` is the interface crate's type for a waiter's
priority or reservation. It is part of the trait from the start so that
the owned wait queue does not need to know which classes exist.

## 5. How the kernel reaches it

The scheduler is called from everywhere in the core: the wake paths, the
futex and address-wait tables, IPC's direct switch, the timer. Those are
compiled before the binary that chooses the classes, so they cannot name
the stack's type. KERNEL-CRATES §6.3 already settled what to do in that
case ("a caller that cannot name it: a link-time binding"):

1. `sched/class.rs` in the core declares the seam with the existing
   `seam!` macro: one list of the operations above as
   `__ferrix_sched_<op>` symbols, with safe wrappers the scheduler calls,
   and a `bind_scheduler!(Stack)` macro.
2. The binary's root binds it once per profile, under the flags:
   ```rust
   #[cfg(all(ferrix_sched_fair = "present", ferrix_sched_fifo = "present",
             ferrix_sched_edf = "absent"))]
   ferrix_kernel_core::bind_scheduler!(Over<ferrix_sched_fifo::Fifo, ferrix_sched_fair::Fair>);
   ```
   None or two invocations do not link, so the linker is the registration
   check, as for `Setters`.
3. **Per-task state lives with the stack, not in `Task`.** `Task`'s layout
   cannot depend on which classes are compiled without making every core
   type generic. The bound stack owns a table of `Entity` values indexed by
   the task's slot, made at boot with the task limit's capacity, and the
   core passes the slot. This keeps `Task` one layout for every profile.
4. **The cost** is one direct, out-of-line call a scheduler operation
   instead of today's inlined calls: a few nanoseconds, measured against
   the IPC round trip (the `optimize-ipc-round-trip` budget) before
   landing. If it is too much on the direct-switch path, that one path may
   take the stack's type through a generic entry instantiated in the binary
   (the §6.3 "caller that names it" shape), since the binary does reach
   it.

## 6. Flags, modes and profiles

* **One flag per class**: `sched_fair`, `sched_fifo`, `sched_edf`,
  `sched_windows`, each `present` or `absent`, each value declaring the
  highest level it allows, as every flag does. The present `schedule`
  flag (`fair` | `edf` | `windows`) is replaced by them; the profiles keep
  their meaning:

  | Profile | fair | fifo | edf | windows |
  |---|---|---|---|---|
  | `max` | absent or present (§7) | absent | present | present |
  | `fast` | present | present | present | absent |
  | `desktop` | present | present | present | absent |
  | `present` (today) | present | absent | absent | absent |

* **Modes are the run-time choice among compiled classes.** `Throughput`
  needs fair; `SoftRt` needs fifo and fair; `HardRt` needs edf. A domain
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
4. **The direct-switch path** (§5.4): accept the out-of-line call, or
   specialise that path.

## 8. What it costs, and what it would cost with Cargo features

**This design:** moving today's scheduler behind the seam is a refactor of
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
2. `ferrix-sched-class` and the seam, with `ferrix-sched` moved to
   `ferrix-sched-fair` behind it. No behaviour change; the IPC round trip
   measured before and after.
3. The per-class flags replacing `schedule`, with the profile rules and
   their controls.
4. Then stage 14's classes, each as its own crate and landing, in
   REALTIME.md §4's order: `ferrix-sched-fifo` with SoftRt, then
   `ferrix-sched-edf` with HardRt, then `ferrix-sched-windows` with M3.
