# Implementation work orders

For the agent building the things, not the one assessing them. Each order below
is scoped so it can be landed on its own with the tree green.

[TODO.md](TODO.md) is the audit-side companion: what to re-measure and how to
know a finding closed. This file is what to write.

Everything here is scoped to the item in [ITEM.md](ITEM.md). Finding ids are
[FINDINGS.md](FINDINGS.md).

---

## 0. Before the first change

**Run `cargo xtask check` once.** It has never been run on this branch. The ten
cross-target clippy passes and the host test suite have not seen the two new
gates in `check.rs` or the env-var branch in `qemu.rs`. Fix whatever falls out
before starting real work, so a later failure is attributable to your change.

**House rules that will otherwise cost you a rewrite.** `docs/CONVENTIONS.md`
is the authority; the load-bearing ones:

* One author per commit. **No `Co-authored-by:` trailer**, no tool signature,
  no "Generated with" line. Enforced by a hook and a CI job.
* The lint table denies `unwrap`, `expect`, `panic`, `unreachable`, indexing
  and string slicing in production code. An exemption is an `#[expect]` whose
  reason begins `AUDIT:`, and it fails once the lint stops firing.
* Every `unsafe` block gets a `SAFETY:` comment and does **one** operation;
  every `unsafe fn` gets a `# Safety` section. `check-unsafe-audit.py` enforces
  it and prints per-crate counts, so growth is visible.
* Commit messages are a subject, a blank line, and a body that argues the
  *why*. Look at `git log` before writing one.
* Generic kernel code never names an architecture; `cfg(target_arch)` lives
  only under `arch/`. Reach architecture code through the `crate::arch` facade.

**Verify gates by their output, not a pipeline's exit status.** `gate | tail &&
commit` commits on failure. Check the status and the text separately.

---

## W-1 — Split `Process` into a core object and a POSIX extension

**Done 2026-09-26.** F-01 and F-06 closed; F-09 re-scoped. The design as built
is below, then the trap and the measurement that shaped it, kept because the
trap is still the obvious wrong change.

**Closes:** F-01 (10 references), F-06 (3). **Size:** large.

### The design as built

Three decisions, each answering one link of the `Task -> Thread -> Process`
chain.

**1. The core process is a type of its own, and the POSIX process contains
it.** `src/kernel/src/object/process.rs` holds `Process { space, pid, started,
handles, membership, counted, exit }` -- what the core enforces or reports,
and nothing else -- with `Exit`, `ProcessRef` and `Control`, which are what a
native handle to a process holds. The personality's `syscall::process::
Process` has it as its *first* field (so it drops first, as the pid and the
job count were given back before the descriptors closed) and implements
`Deref` to it, so `process.space()` and `process.pid()` read as they did at the
personality's call sites; one, a `Process::pid` path in `syscall/mod.rs`, had
to become a closure. The
core type has no field leading back to the extension: not a typed one, and
not a type-erased one either.

**2. Where the core must hold a process whole, it holds a `Host`.** A job kill
walks every process; a native handle to an unstarted process must kill it
when the last handle goes; the pid table must find processes by number. Each
needs the *whole* process -- whose ending closes descriptors and tells a
parent -- without naming what that is. `object::process::Host` is the
personality's object seen through the five questions the core asks of it:

| `Host` method | Asked by | The personality answers with |
|---|---|---|
| `core()` | everything | its core half |
| `kill(status)` | `Job::kill`, `Control`'s drop | `syscall::process::kill` |
| `thread_starting()` | `sched::prepare_user` | its live-thread count |
| `thread_gone(ended)` | `sched`, a prepared task dropped unlaunched | the count, and the end or release it triggers |
| `wait_interrupted()` | `futex` waits | `signal_pending` |

`Arc<PosixProcess>` coerces to `Arc<dyn Host>`, and the personality has its
own type back by `object::process::downcast` (`Any`, a type-id compare). The
pid table moved into the core with it -- the numbers, their cyclic
allocation, and a weak `Host` per number -- because a job kill has to find
every process and could not name the item-ring registry to do it.
`syscall/registry.rs` kept only the personality's typed view and the Linux
rule that thread ids share the pid space, and moved to `load` in a commit of
its own, since that is what it now is.

**3. The scheduler holds a `UserThread`, and the POSIX thread stays the
personality's.** This was the real design question. A thread is what the
scheduler schedules, which argued for the core; but every field of
`syscall::thread::Thread` beyond the process reference is POSIX -- the thread
id from the pid space, the signals sent to it alone, the mask, the address
`set_tid_address` registered. The scheduler used none of them. What it needs
is the process the thread runs in, to count it starting and gone, and the
reference that keeps the thread alive while the task is. That is
`sched::UserThread`, a trait with one method, `process() -> &dyn Host`;
`sched::Task` holds `Arc<dyn UserThread>`, and `thread::of_task` downcasts it
back to the POSIX thread on the syscall path, where it used to clone an `Arc`.

So the chain is now `Task (core) -> dyn UserThread (core) -> dyn Host (core)`,
with the concrete `Thread` and `Process` behind the two trait objects, and no
core file names the personality.

**Lock order and the preemption rule** are unchanged: `Host::kill` is the same
`end` as before, reached from the same places; the pid table is the same
`SpinLock` with the same "drop outside the lock" rule. One thing did change:
entries are now compared by address (`ptr::addr_eq` on the weak pointer)
rather than by upgrading, because an upgrade under the table lock could
produce a process's last reference and dropping a process takes that lock.

### What is left, and where it is filed

`check-item-boundary.py` went from 36 references to 29 (48 to 41 when first
measured, before F-04 and F-08 closed; all four are the gate's counts of the
day, which were lower bounds -- FINDINGS.md §A): seven removed (both
core references to `syscall::process`, all three of F-06, `futex.rs`'s, and
`registry.rs`'s by the ring move). Six item-ring files still name
`syscall::process`, and they do so for POSIX *state*, not for the core
concept, so they are no longer F-01's:

| File | Why it names the POSIX process | Now filed as |
|---|---|---|
| `syscall/mod.rs` | it is the Linux dispatcher: `current()`, `exit_group` | F-09 |
| `syscall/thread.rs` | the POSIX thread holds its POSIX process and its signal state | F-09 |
| `syscall/memory.rs` | `brk` is the POSIX heap; `mmap` of a file needs the fd table | F-09 |
| `syscall/limits.rs` | rlimits read the fd table and the credentials | F-09 |
| `syscall/system.rs` | `sethostname` checks credentials | F-09 |
| `syscall/native.rs` | process creation goes through the Linux loader | F-07 |

Each is a Linux-personality syscall sitting in the item ring, which is F-09's
defect exactly. The next step for them is not another trait on `Host` -- that
would pull POSIX questions into the core's interface -- but deciding, file by
file, whether the item ring should hold them at all. `syscall/thread.rs` is the
clearest case: after this change nothing in the core or the item needs the
POSIX thread except the Linux dispatcher, and it belongs in `load` once the
dispatcher does. It was not moved here because `syscall/mod.rs` names it by a
module-relative path the gate cannot see, and a move that hides an edge is not
a fix.

W-5 took that step (2026-09-26), with a gate that sees module-relative paths:
the Linux dispatcher's routing went above the item first, and then the five
files, with no edge left behind.

### The trap

The obvious reading of F-01 is "move `Process` to `src/kernel/src/object/
process.rs`". **That is the wrong change and it would make the item worse.**
`Process` is not a clean core type. Read `src/kernel/src/syscall/process.rs:65` —
its fields include:

| Field | Belongs to |
|---|---|
| `space: Arc<AddressSpace>` | **core** |
| `pid: u32` | **core** |
| `handles: SpinLock<HandleTable>` | **core** (native ABI) |
| `started: u64` | **core** |
| `umask: AtomicU32` | personality |
| `identity: SpinLock<Identity>` | personality |
| `files: Arc<SpinLock<FdTable<Arc<OpenFile>>>>` | personality |
| `fs: Arc<SpinLock<Context>>` | personality (root and cwd) |
| `state: SpinLock<State>` | personality (signal masks, `brk`) |

Moving the type wholesale would drag the file-descriptor table, the filesystem
context and the signal state into the trusted core — the exact inversion the
boundary exists to prevent. `check-item-boundary.py` would go green while the
item got structurally worse, which is the failure mode worth naming loudest.

### The change, as first written

Split it. A core object holding what the core enforces, and a personality
extension holding what POSIX needs.

1. `src/kernel/src/object/process.rs` — `Process { space, pid, started, handles }`,
   plus `pid()`, `space()`, `with_handles()` and job membership. This is what
   `object/`, `sched/` and `trap.rs` consume.
2. The personality keeps a `PosixProcess` (name it as the tree prefers) holding
   `umask`, `identity`, `files`, `fs`, `state` and the `brk` lock, reached from
   the core `Process` by a handle or a side table the personality owns — **not**
   by a field on the core type, which would restore the dependency in the other
   direction.
3. `current()` moves with the core type.

### What the measurement said (2026-09-25)

Investigated properly before starting, and the shape is different from what
F-01's wording suggests.

**The core-facing interface is small.** Everything the trusted rings actually
need from `Process` is about seven operations:

| Caller | Needs |
|---|---|
| `sched/mod.rs` | `space()`, `thread_starting()`, `thread_gone(bool)` |
| `object/job.rs` | `move_to(job)`, and `process::kill` |
| `object/mod.rs` | `ProcessRef::exit()` |
| `trap.rs` | `current()`, `pid()`, `space()` |

Seven operations against a type with 78 methods. A core-side trait would be
cheap, and `Arc<Process>` coerces to `Arc<dyn CoreProcess>` without touching
the personality.

**But the ownership chain crosses two boundaries, not one.** `sched::Task` does
not hold a `Process`: `task.rs:450` is
`self.thread.as_ref().map(|thread| thread.process())`. It holds a **`Thread`**,
and `Thread` holds `Arc<Process>`. So the chain is

    Task (core) -> Thread (item) -> Process (load)

which is why the register carries both F-01 and F-06 and why closing either
alone does not help. The 34 personality callers all go through
`Thread::process()`, so they are *not* affected by a change to `Task` — that is
the good news, and it is what makes a trait viable at all.

**Consequence for the work order.** A correct fix needs a core-side abstraction
for `Thread` as well as for `Process`, and `Thread` in turn names
`syscall::process` and `syscall::signal`. Deciding where `Thread` belongs is
the real design question: a thread is what the scheduler schedules, which
argues for the core, but `syscall/thread.rs` reaches into the personality for
signals and process state.

That is an architectural restructuring of the process/thread/task ownership
model, not a file move. It wants a deliberate design pass, and it is the reason
this order remains open after a session that closed eleven other findings.

### How it landed

The order above (a re-export first, one consumer per commit) assumed the type
would move; it was split instead, which cannot be done a consumer at a time.
Four code commits, each green, then the documents:

1. `object::process` with the core fields, `Exit`, `ProcessRef`, `Control`,
   `Host` and the pid table; the POSIX process contains the core one; `object/
   mod.rs` and `object/job.rs` switch. Retires 3 (F-01 ×2, F-06 ×1).
2. `sched::UserThread`; `Task` holds one. Retires 2 (F-06).
3. `Host::wait_interrupted`; `futex.rs` takes a `Host`. Retires 1 (F-01).
4. `syscall/registry.rs` to `load`. Retires 1 (F-01). A boundary change,
   argued in its own commit so it can be judged -- or reverted -- on its own.

### Verify

`python3 tools/common/check/check-item-boundary.py --report` shows no `F-01` or `F-06`
entry and nothing from `object/` or `sched/` above the core. Every boot gate
exercises this code: `object/check.rs` and `sched/check.rs` on every boot,
`test-threads` for the thread path, `test-jobs` for job control, and `test-shell`
for fork, exec and wait.

### Pitfall

`docs/sysml/` describes the object model; `06-objects.sysml` now has `Process`
(core), `PosixProcess :> Process` and `Thread`, and `05-scheduling.sysml`
`UserThread`. `gen-arch-doc.py --check` fails if the model and the generated
document disagree.

---

## W-2 — Invert the trap-return upcall

**Done 2026-09-25.** See F-02 and F-02a.

**Closes:** F-02 (7 references). **Size:** small. **Value:** high — it is on
the most trusted path in the system.

Three functions are called from `arch/*/signal.rs`, `arch/*/trap.rs`,
`arch/x86_64/syscall.rs` and `trap.rs`:

```rust
needs_attention() -> bool
return_to_user(context: &mut arch::UserContext)
sigreturn(context: &mut arch::UserContext, rt: bool)
```

Simple signatures, so the inversion is cheap. Define the interface in the core
— a struct of three function pointers, or a trait object behind a
`SpinLock<Option<_>>` — and have the personality register into it during init.
`arch/` then names only the core interface.

**Verify:** the core no longer names `crate::syscall::deliver`. A kernel built
without the personality registered must still return to user mode for a process
that has no signals pending; if it cannot, the interface is not actually
inverted.

**Pitfall:** this is on the return-to-user path, so an indirect call costs on
every trap. Measure it — the roadmap (`docs/roadmap/`) tracks boot cost lines and the
project cares about this. If it shows, an `Option<fn>` checked once beats a
trait object.

---

## W-3 — Invert `StatLayout`

**Done 2026-09-25.** See F-03.

**Closes:** F-03 (3 references). **Size:** tiny. Good first task.

Each `arch/*/mod.rs` declares `STAT_LAYOUT: crate::syscall::stat::StatLayout`.
Invert it: the personality asks the `crate::arch` facade which layout the ABI
wants, via an arch-owned enum or a plain discriminant the personality maps.

---

## W-4 — Registration for board support and the block ring

**Done 2026-09-26.** F-05 closed on 2026-09-25 and F-04 on 2026-09-26, and
F-08, which had no order of its own, went with it: power, init and `devmgr`
took the same shape, an interface the item defines and the load ring
registers into. `src/kernel/src/hooks.rs` is the list type the item keeps
registrations in, and `main.rs`'s `register_load` is the one place they are
made, in bring-up order, with a check that each was (FX-0006). FINDINGS.md
F-04 and F-08 say what moved and what the gate still cannot see.

**Closes:** F-04 (6), F-05 (1), and F-08 (6). **Size:** small.

`device.rs` names `stm32mp1`, `platform::st::stm32mp1::gpu`, `platform::st::stm32mp1::usb`; `claim.rs` names
`block_ring`. Both want the dependency the other way: board support registers
itself with the core registry at init instead of the registry naming each
board.

**Pitfall:** registration must happen before the first consumer runs.
`main.rs` and `init.rs` own bring-up order — put the call there explicitly
rather than relying on a link-time trick, which is unanalysable and would be a
finding of its own.

---

## W-5 — A registration table for the native dispatcher

**Done 2026-09-26.** F-07, F-09 and F-33 closed: 56 references, and the debt
register is empty. The order was written for F-07's 12; the resolving gate
sized it at 56, and the same pattern answered most of them.

**Closes:** F-07 (12 references), F-09 (39), F-33 (5). **Size:** medium.

### The design as built

Six commits, each green, then the documents.

1. **The core's syscall entry.** `SyscallArgs` and `Outcome` moved into
   `trap.rs`; the three architectures call `trap::system_call`, which
   answers through a `SyscallEntry` in a `Once` that `main.rs` points at
   `syscall::dispatch`, beside the personality's `ReturnPath`. Retires F-09's
   three `arch` entries.
2. **The native table.** `native.rs` answers the calls on the core's objects
   in its exhaustive `match`, as before. The six about a subsystem above the
   item go to a table of `Handler`s the subsystems register with
   `native::serve` -- the rings, the display, the renderer and input through
   an `install` each, cgroupfs through `fs::install` -- and the device handle,
   its right and the driver's handle stay in the item
   (`native::control_channel`). Process creation and start, for the ABI and
   for `devmgr`, go through `native::Processes`, which `syscall/launch.rs`
   lends; a quiesce waits out registered `Server`s. Retires F-07's 12.
3. **The Linux dispatcher above the item.** `syscall/mod.rs` keeps the way in,
   the native range and the decode with its clamp, and hands the decoded call
   to a `Personality` -- a trait it defines -- that `syscall/linux.rs`
   implements. Retires 21 of F-09. First held in a pointer; then, measured,
   composed by `main.rs` at compile time (`dispatch_with::<Linux>`) so it
   costs no second indirect call.
4. **Five files to `load`**, by the manifest alone: `futex`, `limits`,
   `memory`, `system`, `thread`, each argued in ITEM.md §2. Retires 15 of F-09.
5. **The paranoid check to a verification file**, `arch/x86_64/paranoid/
   check.rs`. Retires F-33's 5.

`main.rs` gains five composition-root edges (`block_ring`, `net_ring`,
`render`, `input`, `syscall::linux`), each a registration call, and its
registration check covers the new interfaces.

### The exhaustiveness the `match` gave

This order's pitfall, and it is kept rather than traded: the calls the item
answers are still in an exhaustive `match`, so an unanswered one does not
compile. Only the six the table holds are checked at boot instead, on every
boot, before anything can make a native call (FX-0006). The check is in
`main.rs`'s `register_load`, with the other registrations, rather than in
`syscall/check.rs`: that file is load-ring verification, and the check is the
item holding the load to its registrations.

### Cost

Dispatch is the hottest path, so nothing on it locks or allocates. Every
call pays one indirect call through the core's `Once`; a native call to the
table a short search by decoded call besides. Measured under KVM on a Zen 5
host with busybox `dd bs=1` copying a million bytes from `/dev/zero` to
`/dev/null` -- two million `read`/`write` calls -- nine times a boot, six boots
alternating with the series' base:

| | min | median | mean |
|---|---:|---:|---:|
| base | 1.074 s | 1.185 s | 1.279 s |
| personality behind a second pointer | 1.112 s | 1.244 s | 1.274 s |
| base | 1.081 s | 1.242 s | 1.324 s |
| personality composed at compile time | 1.100 s | 1.257 s | 1.330 s |

The second pointer cost a median +5.0%, so it went; what is left is +1.2%
(minimum +1.8%), inside the noise of a host running other sessions at a load
of about 20. Not committed: the benchmark is a line added to `test-vfs`'s
command list for the run.

### Verify

`python3 tools/common/check/check-item-boundary.py --report` shows no upward reference and
an empty register. The full boot gate row, `test-threads`, `test-jobs`,
`test-net` and `test-boot --mitigations off`: every native call devmgr and its
drivers make, every Linux call busybox makes, and the paranoid check's
breakpoints, go through the new paths.

---

## W-6 — Complexity and recursion gate

**Done 2026-09-25.** See F-25. **Corrected 2026-09-26:** the gate's string
stripping mis-paired quotes after a `\`-newline continuation and left 328 of
the item's 1,887 functions unmeasured. It now reads code through
`tools/common/check/rustlex.py`, shared with the boundary gate, and the baseline was
re-recorded at 47 entries.

**Closes:** F-25. **Size:** medium. No kernel changes.

The one code gate the audit did not build. EN 50716 requires a coding standard
with metrics; eleven gates enforce other properties and none bounds cyclomatic
complexity, function length or recursion.

Follow `tools/common/check/check-item-boundary.py` exactly — it is the current best
example of the ratchet pattern: measure, record a baseline, refuse growth,
fail on stale entries.

* Complexity: without a Rust parser, approximate by counting branch points per
  function (`if`, `match` arms, `while`, `for`, `&&`, `||`, `?`). **Say in the
  docstring that it is an approximation.** The house rule is that a number
  whose caveats travel separately is worse than none.
* Recursion: build a call graph from function names within a crate and report
  cycles. Direct recursion is easy and worth catching; mutual recursion through
  trait objects is not detectable this way, and the docstring must say so.
* Wire into `cargo xtask check` after the item-boundary step.

**Pitfall:** the baseline will be large. Do not tune thresholds until the item
is below them — record what exists, then ratchet.

---

## W-7 — Finish the coverage story

**Done 2026-09-26**, except that F-10 stays open on the statements that need a
test. Steps 1 to 5 are done: every architecture and both profiles measured,
every boot gate that exercises the item in the union on every architecture
(thirteen on x86-64, nine on ARMv7-A, and on AArch64 the same nine plus a
`test-boot` on a GICv3), the residual sorted per architecture, and the ratchet
wired up as `cargo xtask coverage`. Doing step 3 found two defects in
`coverage-report.py` that had made the published 81.9% wrong;
VERIFICATION.md §3.4. The corrected figures, on main at a6d505a2, are 74.7%
(x86-64), 73.7% (AArch64) and 70.9% (ARMv7-A).

**Closes:** F-10, F-11, F-12. **Size:** medium, mostly running things.

Reproduction, with QEMU's drcov plugin built from its source tree:

```
FERRIX_DRCOV=/home/johndoe/Documents/qemu/qemu/build/contrib/plugins/libdrcov.so \
  cargo xtask coverage --arch x86_64 \
    --init "$HOME/.local/share/ferrix/busybox/{arch}/bin/busybox.static"
```

It runs each gate with `--accel tcg` (a TCG plugin observes nothing under KVM)
and `--smp 2` on ARMv7-A, keeps every boot's trace and the kernel it ran in
`build/coverage/<arch>`, and runs `coverage-report.py` over the lot against the
floor in `coverage-floor.json`. One gate by hand is still
`FERRIX_QEMU_PLUGIN="<libdrcov.so>,filename=<dir>/x.drcov" cargo xtask <gate>
--accel tcg`.

1. **AArch64 and ARMv7-A** (F-12) — done, and since 2026-09-26 the suite.
2. **Release profile** (F-11) — done, one boot: 75.2% against debug's 71.6%.
3. **More gates in the union** (F-10) — done. The gates that ended by killing
   QEMU are now asked to stop first, which lets the plugin write its table,
   and each boot of a gate keeps its own trace. Not in the union, and why:
   VERIFICATION.md §3.5 (`test-seat`, `test-compositor`, and the gates
   needing a GL host or fetched volumes). `test-vfs` joined the Arm pair's
   suite on 2026-09-27.
4. **Enumerate the residual** — done per architecture: COVERAGE-RESIDUAL.md
   sorts it, COVERAGE-WORKLIST.md groups the *needs a test* category by module.
5. **Ratchet it** — done as `cargo xtask coverage`, not in `cargo xtask
   check` since it needs boots, and not in CI, whose packaged QEMU carries no
   drcov plugin (the one used here is built from QEMU's source tree). Floors
   are the measured figure less a point.

**What is left is F-10's test-writing**: 77 statements on x86-64, 146 on
AArch64 and 130 on ARMv7-A (2026-09-27), by module in COVERAGE-WORKLIST.md. Take a module,
write the tests, re-run `cargo xtask coverage`, regenerate the evidence and
raise the floor. A statement no run of the measured machine can reach gets its
argument in `coverage-argued-<arch>.json` instead, which the generator checks
against the residual.

**Advanced 2026-09-26 on x86-64** (main at 195a2e93): `arch/x86_64`, x86-64's
share of `arch`, `trap` and `smp` have nothing left that needs a test -- 216,
8, 34 and 47 statements before, covered by new stage 3 and stage 4 checks and
three more boots in the suite (`boot-legacy`, `boot-reset`, `boot-single`),
or argued statement by statement (74). Two more tool defects were found
doing it (VERIFICATION.md §3.4), which is most of x86-64's move from 74.7% to
82.2%. The floor is raised to 81.0.

**Advanced 2026-09-27 on every architecture**: the memory layer and the
objects, the Arm architectures' code and the kernel's services, one pass
each, with the checks FINDINGS.md F-10 lists and seven more suite boots on the
Arm pair and one on every architecture (`boot-options`). Measured together:
89.5% on x86-64, 90.2% on AArch64, 84.8% on ARMv7-A, the rest argued in 275,
264 and 329 per-statement arguments or put down to absent hardware, and 77,
146 and 130 statements still needing a test. The floors are raised to 88.5,
89.0 and 83.5.

---

## W-8 — Low-level requirements, and ids on assertions

**Closes:** F-14, F-15, F-16. **Size:** large. **The biggest structural gap.**

**Step 1 done 2026-09-27**: the format and the gate. `ItemHighLevel` and
`ItemLowLevel` are defined in `docs/sysml/13-item-requirements.sysml`, with
`statement`, `criterion`, `parent` and (low level) `unit` as string
attributes; the model reader learned string values that span lines and hold
the notation's own punctuation. `tools/common/check/check-traceability.py` runs in
`cargo xtask check` with `--check`, self-tests on every run, keeps the
baseline in `tools/common/data/traceability-baseline.json` and writes
`docs/certification/TRACEABILITY.md`. Two departures from the design below:
a `/// Verifies:` tag on an xtask gate goes on the function in `tools/common/xtask/src`
that implements the gate; and the run-time column needed evidence the coverage
run did not keep -- `coverage-report.py` dropped the check files' statements
as not the item's -- so it now records them apart, in the `verification` map
of `coverage-<arch>.json`, which `carry-coverage.py` carries. Until the next
`cargo xtask coverage` writes that map, the matrix says *not measured*.

**Step 2 done 2026-09-27**: 51 high-level requirements in nine areas --
`H.MEM` 11, `H.OBJ` 9, `H.SCHED` 5, `H.IRQ` 3, `H.DMA` 5, `H.TRAP` 7,
`H.BOOT` 4, `H.QUOTA` 5, `H.FAIL` 2 -- the eight the design named and
`FAIL` for O.FAILSAFE's safe state, which no other area owns. Each of the
eight objectives and eight ASRs is the parent of at least one
(TRACEABILITY.md, "From the system level"). ASR-8's admission control is not
built (AoU-4), so no requirement asks for it; the scheduling area requires
only what the fair class does. One is tagged, `H.MEM.7`, on the two
shootdown checks in `smp/check.rs`; the other 50 are the baseline, left to
the per-subsystem slices. What step 3 should know:

* several of the boot's strongest checks are not in a check file -- the
  `w^x` and `sealed` sweeps, `check_stacks`, `check_iommu` are functions of
  `main.rs`, product code -- so a tag cannot go on them. Either the gate
  learns a marker for them or they move into a check file; moving is the
  cleaner answer, since the item's size then stops counting them;
* a tag credits a whole requirement, and a requirement whose criterion has
  two halves (`H.MEM.7`: tables *and* frames) needs a check for each; the
  slice that tags should say which half each check covers in the commit;
* a first look finds checks that appear to discharge more of them, left
  untagged here for their slices to read and argue: `object/quota_check.rs`'s
  `check_the_counters` for `H.QUOTA.5` (a parent's limit refusing through a
  child's), `arch/x86_64/trap/check.rs`'s forged i386 frames for the x86-64
  part of `H.TRAP.5`, `arch/x86_64/gdt/check.rs` for `H.TRAP.7`. A criterion
  no check tests is check-writing work, as the design says, not an argument.

**Step 3 done 2026-09-27: the pilot, `object/`.** 103 low-level
requirements, `L.object.1` to `L.object.103`, in
`docs/sysml/14-object-requirements.sysml`, one package per file of
`src/kernel/src/object/`. **78 are verified**, by 49 check functions and 4 host
tests newly tagged (and one tag added to a check tagged already) in object/'s own check files and in
`syscall/native_check.rs`, `syscall/init_calls_check.rs`,
`fs/cgroupfs/{native,delegation,controllers,oom}_check.rs`,
`fs/kmem_check.rs` and `src/lib/kernel/objects`; **25 need a check** and are
the baseline's `L.object.*` (TRACEABILITY.md lists them). Of object/'s 261
product functions, 173 are a requirement's unit, 83 are accessors and 5 are
check code in a product file; **0 are named by none**, and the gate now holds
`object` as complete, so a function added there without a requirement fails
`cargo xtask check`. The high level grew from 51 to 63: four behaviours
object/ has that nothing said (`H.OBJ.10` bounded queues, `H.OBJ.12` the job
tree as the cgroup hierarchy, `H.OBJ.13` pids, `H.QUOTA.6` the scoped OOM
kill), and eight halves split out of `H.OBJ.3`, `4`, `8`, `11`,
`H.QUOTA.1`, `2`, `3` and `4` so one check can prove each (`H.OBJ.14` to
`17`, `H.QUOTA.7` to `9`). 18 of the 63 are verified.

The 25 that need a check, as check-writing work: the port and passive
kinds' signals (`.2`); dispose's depth bound and the orphan queue's give-up
(`.5`, `.8`); a write refused for PEER_CLOSED, TOO_BIG or a full queue
keeping its handles (`.12`); messages written before a close read before
PEER_CLOSED (`.19`); 64 registrations (`.34`); masking read back at the
controller or MSI-X entry, and an unheld line masked (`.41`, `.42`); the
quarantine's three (`.47` to `.49`), whose check `pin::check_quarantine`
exists but lives in `pin.rs`; a reused quota slot claimed clean (`.54`);
`charging_nobody` (`.58`); `cpu.weight`'s clamp and its effect on a busy
parent (`.62`); a child job charged as an object and refused in a killed
parent (`.65`); `cgroup.max.depth` and `.descendants` (`.67`); rmdir of a
populated cgroup (`.70`); a fork into a dying job (`.78`); a thread charged
as a task (`.84`); process_create at the task limit (`.87`); a bootstrap
that found the table full (`.92`, verified since 2026-09-27 by the native
refusal check, its second half split out as `.104`, a bootstrap closed when
its process ends before it is placed); a started process held by its task
(`.97`); and the OOM kill's choice of victim, its fallbacks and the emptying
of an ended victim (`.101` to `.103`).

Closed since, by the F-10 coverage slices of 2026-09-27: `.92`'s first half
(above); `.8`, a give-up past the in-place depth, by
`alloc_check::give_up`, which loses its chain's last end on purpose and so
runs after every check that counts what was given back, last before the
boot's marker; `.62`, by `quota_check::check_the_weight`; and `.103`, by
`quota_check::check_an_ended_victim_is_emptied`, which drives the faults'
retries a step at a time. `.105` was added for what `defer` does below the
depth, a drop in place, with its check `check_a_close_without_queue_room_drops_in_place`.

What the pilot taught, for the slices that copy it:

* **One check proves a whole requirement.** A `Verifies:` tag credits the
  whole requirement, so a check names one only if it proves its entire
  criterion by itself -- never the union of two checks, since removing
  either would leave the matrix claiming what nothing tests. Where the
  checks that exist each prove a part, the requirement is split, one part
  per check (the coordinator's rule, from a peer session that declined to
  tag `H.TRAP.1` from a check of one pointer kind of three). The pilot's
  first draft tagged some requirements from two or three partial checks; a
  second pass split 13 low-level and 8 high-level requirements.
* **The statement says no more than the criterion tests**, at both levels.
  A statement broader than its criterion is a tag crediting what no check
  proves; the pilot found it in step 2's `H.OBJ.8` (every object kind; one
  check of unstarted processes), `H.QUOTA.1` (the refusal's EAGAIN and
  SHOULD_WAIT answers), `H.QUOTA.3` (VMOs, jobs and pins; the check makes
  ports and channels) and `H.SCHED.3` (weights in proportion; the check has
  equal weights). The first three are object/'s and are split or untagged
  here. **Proposed for other slices, not done:** `H.SCHED.3` into equal
  weights against many tasks (verified by `quota_check::check_the_processor`
  today) and unequal weights (no check); `H.OBJ.2` (every native operation
  that needs a right) into a sweep check or one requirement per class of
  call, since `syscall/native_check.rs` tries one; `H.OBJ.5` (the refused
  writes that keep their handles, no check -- `L.object.12`); `H.IRQ.1`,
  whose criterion says 16 of 16 wakes where the check requires 5 of 8;
  `H.QUOTA.5`, whose criterion needs a grandparent's limit where
  `check_the_counters` limits the parent (relax it to "an ancestor", or set
  the limit two levels up).
* **A requirement names every function that carries its behaviour** as its
  `unit` -- a list, not one each. A write that is all or nothing is one
  sentence whatever number of functions make it so.
* **Accessors need no requirement of their own.** The gate's rule, with a
  self-test: a body of one statement or one expression, no branch point as
  `check-complexity.py` counts them, and no `unsafe`. 688 of the item's
  2,335 product functions are accessors; they are counted, not dropped, and
  a requirement may still name one where it is the behaviour's entry point
  (object/ names 31).
* **Check code in a product file is listed, not required.**
  `tools/common/data/traceability-units.json` names each with the reason it has
  not moved (5 in object/, the quarantine's check and what serves it); the
  gate fails on a stale entry. Step 2's `main.rs` sweeps go there too until
  they move; moving them is the better answer, since a tag can then go on
  them and the item stops counting them.
* **A criterion that leans on its caller says so.** Two object/ checks
  rely on the frame count `object::check::run` takes around all of them;
  the criterion names it rather than claiming the check counts frames.
* **Ids are flat per subsystem**, `L.<module>.<n>`, with a package per file
  for the reader. Renumber freely while writing; never after a tag exists.
* **The gate read owners wrongly:** an `impl` in a signature
  (`f: impl FnOnce(..)`) was taken for an impl block, and every function
  after it in the file got the wrong owner, so `Process::job` or `Job::kill`
  could not be named. Fixed; a slice that finds a unit that will not resolve
  should suspect the gate before the name.
* **Time.** About 45 minutes of agent time from the first read to the last
  tag: reading object/'s 5,600 lines of product code and 5,000 of checks,
  writing 103 requirements and 12 new high-level ones, and tagging. The
  checks are the cost -- each tag is a criterion read against a check's
  assertions -- about half a minute a requirement once the code is read,
  and the one-check pass added a third to it.

**Step 4, the `iommu` slice, done 2026-09-27.** 43 low-level requirements,
`L.iommu.1` to `L.iommu.43`, in `docs/sysml/16-iommu-requirements.sysml`
(15 is the sched slice's), one package each for discovery, bring-up and
domains, pins, the unit gate and faults. **16 are verified**, by 6 kernel
checks and 2 xtask gates newly tagged and one tag added to
`object/check.rs`'s pin check; **27 need a check** and are the baseline's
`L.iommu.*`. Of iommu's 95 product functions 76 are a requirement's unit
and 19 accessors; **0 are named by none**, and the gate now holds `iommu` as
complete. What moved, output unchanged on all four boots:

* `check_iommu` and `check_dma_faults` out of `main.rs`, and `check_domains`
  (with its body and report) out of `iommu.rs`, into `iommu/check.rs`; the
  quarantine's `check_quarantine`, `check_pin` and `translated_pci_domain`
  out of `object/pin.rs` into `object/pin/check.rs`, a child module that
  reads what `pin.rs` keeps private. That verifies `L.object.47` to `49`
  and `H.DMA.4`, and leaves `Exit::for_check` the only quarantine entry in
  `traceability-units.json`. The placements report `check_iommu` began with
  runs on every boot, checks or not, so it stays product code, as
  `iommu::report`; `kmain` is a line longer for the two calls, recorded in
  `complexity-baseline.json`.
* The high level: `H.DMA.2`'s criterion had three parts with a check for two,
  so it is split into `H.DMA.2` (a write outside the domain is recorded
  against its stream: xtask's `fault_problem`), `H.DMA.6` (no fault no check
  provoked: `check_dma_faults`) and `H.DMA.7` (the refused write leaves the
  page as it was: no check). `H.DMA.3` into `H.DMA.3` (translated only while
  pinned, the two refusals: `pin_and_unpin`) and `H.DMA.8` (invalidation
  before a frame or an emptied table is reused: no check); its "through a
  handle its driver holds" is `H.OBJ.1` and `2`'s. 22 of 66 high-level
  requirements are verified.

The 27 that need a check, as check-writing work: the units found and their
counts (`.1`); each firmware's placement, which the boot prints and no gate
reads, and the unresolved cases, which no QEMU produces (`.3` to `.6`); a
unit left alone (`.8`); a function with no domain reaching nothing (`.9`);
an untranslated domain where no unit is, and the degraded-mode line once
(`.11`, `.12`); detach giving back what attach took, a domain in use staying
attached, a refused attach leaving nothing (`.15` to `.17`); the SMMUv3's
MSI doorbell (`.18`); a frame past 39 bits, a part-done pin undone (`.23`,
`.25`); the unit's invalidation and the emptied tables before unpin returns
(`.26`, `.27`, the F-36 order on the device side); caching mode (`.28`); a
waiter woken at the gate's leave and a unit that never answers (`.32`,
`.33`); the fault readers' overflow and event cases and firmware's leftover
fault (`.37` to `.39`); and the fault accounting's negative controls --
provoke, not-stray, stray, the audit finding one (`.40` to `.43`). Most want
a unit that misbehaves on purpose, which QEMU does not offer; a software
double of a unit's register file would reach `.8`, `.26`, `.33` and `.37` to
`.39` at once.

What this slice found, for review:

* **Most of a unit is proved only end to end**, by the out-of-domain probe
  in `pci/virtio.rs`, which is product code. The tag is on what judges its
  count, `tools/common/xtask/src/qemu.rs`'s `fault_problem`: `.7`, `.10`, `.35`, `.36`
  and `H.DMA.2`. It skips on an AArch64 boot whose PCI came from the device
  tree and on a QEMU without SMMUv3 stage 2, and passes there without
  proving anything. Moving the probe into a check file is the pci slice's
  question.
* **`H.DMA.1` stays unverified by one line**: `iommu_problem` requires 1 or
  more functions behind a unit and 0 unresolved, not the criterion's 0
  bypassing. Adding that condition would verify it.
* **Discovery is a second reading of firmware**: `discover` places functions
  for the report, and `bring_up` with `domain_for` places them for the
  domains, from the same tables by different code. The report's counts
  prove the first; only the probe proves the second, on one function.
* `tools/common/xtask/src/dma_faults.rs`'s `problem` (every VT-d fault traced over a run
  is one the kernel named) is left untagged: with no probe it passes
  vacuously, so it proves `.40` only together with `fault_problem`.
* **Time.** About 45 minutes of agent time from the first read to the
  gates: ten reading the 1,800 lines of iommu product code and its checks,
  fifteen for the move and its before-and-after boots on four
  configurations, twenty for the 43 requirements, the two splits and the
  tags.

**Step 4, the `mm` and `user` slice, done 2026-09-27.** 166 low-level
requirements in `docs/sysml/17-memory-requirements.sysml`, in two id spaces
because they are two layers: `L.mm.1` to `L.mm.61` for the kernel's own
memory (`mm.rs`, `mm/`, `vmap.rs`, `early.rs`) and `L.user.1` to
`L.user.105` for a program's (`user/vmo.rs`, `user/space.rs`). **111 are
verified** -- 27 of `L.mm`, 84 of `L.user` -- by 90 check functions newly
tagged, or given one more id, in `mm/check.rs`, `user/`'s five check files,
`smp/check.rs`, `syscall/{check,unmap_check,vdso_check}.rs`,
`fs/{check,mmap_check,memfd_check}.rs` and `object/{check,alloc_check,
quota_check}.rs`; **55 need a check** and are the baseline's `L.mm.*` and
`L.user.*`. Of the 297 product functions of `mm`, `vmap`, `early` and
`user`, 236 are a requirement's unit, 49 accessors, 12 check code in a
product file (`traceability-units.json` says why each stays), and **0 are
named by none**; the gate holds all four as complete. The item's unnamed
functions go from 1,393 to 1,178.

Before the requirements, the checks moved. Stage 2's whole allocator check
(`memory_check`, the frame, heap, vmap-arena, device-window and stack checks)
and the W^X and sealed sweeps left `main.rs` and `mm.rs` for a new check
file, `mm/check.rs`, run from the same two points of bring-up with the same
boot lines, so a tag can go on them; `mm::sweep` and `permissions_of`, which
only the sweeps and the protection check used, followed. The item counts 21
product functions fewer. Stage 1's checks of the early mapper and stage 3's
demand-paging check are still in `main.rs`: `check_early_mapper` maps the
framebuffer too, so it is not check-only, and the requirements they would
verify (`L.mm.18`, `.58` to `.60`) wait in the baseline.

The high level grew from 66 to 74, by the one-check rule and by behaviour no
requirement said:

* `H.MEM.7` keeps the page tables an unmap empties (on
  `tables_wait_for_their_shootdown`); its remap half is `H.MEM.12`, on
  `smp/check.rs`'s `shootdown`, which was tagged `H.MEM.7` for a half it
  proves; its frame half is `H.MEM.17`, which no check proves.
* `H.MEM.5` keeps the sealed sweep; the direct-map write that must fault is
  `H.MEM.13`, no check. `H.FAIL.2` keeps the guard pages (`check_stacks`);
  an overflow reported as the safe state is `H.FAIL.3`, no check.
* `H.MEM.8` keeps the `cow` program; the `shared` program is `H.MEM.16`.
* `H.MEM.4` said "any page table", and the sweep walks the kernel's root and
  the identity root. It now says those. **User mappings may be writable and
  executable at once** -- `mmap` and `mprotect` pass `PROT_WRITE |
  PROT_EXEC` through, as Linux does -- so O.WXN as the Security Target states
  it covers the kernel's mappings only; what the kernel refuses a program
  (an executable device window or native VMO mapping, a writable vDSO) is
  `H.MEM.18`, no one check yet. Whether RWX user memory needs an assumption
  of use is for the Security Target's owner.
* New: `H.MEM.14`, the kernel's allocators give each frame, block and range
  one holder and never frame 0 (`memory_check`); `H.MEM.15`, the kernel's
  own mappings are what was asked (`check_vmap`).

31 of the 74 are verified. What is left open at the high level from this
slice's areas: `H.MEM.9` names three boot lines no one check proves, and
`H.MEM.10` asks for a range straddling the image's end where stage 2's
check probes one straddling its start.

The 55 that need a check, as check-writing work. Kernel memory: bring-up
against a constructed map (`L.mm.1`), a user table charged to its job and a
disowned frame charged to nobody (`.9`, `.10`), `allocate_frames_below`
(`.11`), a heap refusal counted (`.14`), a table, a `zero_frame` and a
`copy_frame` read whole after the frame was dirtied (`.15` to `.17`, and the
arena's `.45`) -- the checks that exist never dirty the frame first --,
`map_in` and user code fetched as written (`.20`, `.21`), the kernel slots
shared (`.22`), `unmap_unwalked` and `prune_in` (`.24`, `.25`), the I/O
tables (`.26`, `.27`), a kernel unmap's release and its no-memory path
(`.29`, `.30`), the reclaim's counts (`.34`), the reserve's refusal, scope
and large blocks (`.36` to `.38`), merged and dropped table lists (`.40`,
`.41`), `device_windows`, `free_stacks` and `Buffer` (`.54`, `.56`, `.57`),
the early mapper (`.58` to `.61`); and three whose checks exist in product
files -- `check_demand_paging` in `main.rs` (`.18`), vmap's own
`check_failed_device_map` and `check_invariants` (`.52`, `.53`). User
memory: a first write committing a zeroed page and an uncommitted page
reading zeros (`L.user.3`, `.10`), a VMO at the object limit (`.9`, with
`H.QUOTA.9`), a byte range leaving the page (`.11`), a displaced shared
frame taken down and `retire` folding the caller's shootdown (`.13`, `.18`),
held ranges and an undone move (`.28`, `.31`), the mapper list's pruning,
single private mapper and charge (`.33` to `.35`), `insert_absent` (`.38`),
mapped writes reported (`.43`), `with_present_page` (`.65`), `madvise`'s
refusals (`.84`, `.85`), a pending shootdown widening the forget (`.91`),
the ceiling (`.102`), a disk file's page (`.103`, waiting on the ring-3
disk), a fault with no preemption lock held (`.104`), and an unmap's order
at the space level (`.105`).

* **Time.** The requirements were drafted by three agents in parallel, one
  per layer, from the pilot's lessons, in about 20 minutes; review, the
  high-level splits and tagging took about as long again.

**Step 4, the `smp` slice, done 2026-09-27.** 32 low-level requirements,
`L.smp.1` to `L.smp.32`, in `docs/sysml/22-smp-requirements.sysml`, one
package each for the processors and their records, bring-up, the whole-TLB
shootdown and its bounds, the scoped shootdown, grace periods, stopping for
a panic and the scheduler's kick. The start sequences under them,
`arch/<isa>/smp.rs`, are the arch slices'. **9 are verified**, by 4 check
functions newly tagged in `smp/check.rs` (`everywhere`, `page_sets`,
`grace`, `migrating_shootdown`) and one id added to each of 3 tagged
already (`unlink_and_shoot`, `shootdown`, and `user/rmap_check.rs`'s
`protect_under_child`); **23 need a check** and are the baseline's
`L.smp.*`. Of smp's 57 product functions 38 are a requirement's unit (3 of
them accessors named as an entry point: `patience`,
`TlbPages::addresses`, `is_everything`), 17 accessors and 2 check code --
`run_everywhere` and `next_job`, how stage 4's checks hand work to every
processor, which stay because `secondary_main`'s product loop reads the
same state; **0 are named by none**, and the gate holds `smp` as complete.
Doc-comment lines only: nothing moved. smp's 37 functions named by none
(35 now a unit, 2 check code) leave the item with 1,145 of 2,345 on the
main this was rebased onto.

The high level grew from 74 to 77, by behaviour no requirement said, with
no split: `H.SCHED.6`, each processor finds its own record through its
register (verified by `everywhere`); `H.MEM.19`, a grace period outlasts
every read-side section running when it began (verified by `grace`); and
`H.BOOT.5`, every processor firmware describes is running on its own record
before the scheduler starts, or the boot halts (no check). 33 of the 77 are
verified.

The 23 that need a check, as check-writing work: the topology's refusals
of an empty list, a repeated identifier and a list without the boot
processor, and its numbering (`.1`, `.2`);
`this_cpu` before the boot record is installed (`.4`); the record checks
that halt a processor, and `this_cpu_for_report` (`.5`, `.6`); every
secondary online (`.7`) -- the `cpus` line says it on every boot and
`bring_up_processors` halts with FX-0406 otherwise, but no gate reads the
line, and one xtask condition would verify `.7` and `H.BOOT.5` at once --
and a secondary that never reports in (`.8`); the side-channel defences
decided before the first secondary (`.9`) and the IPI handler registered
before it (`.10`); a newcomer's online-then-flush handshake (`.12`); the
scheduler taking every secondary over (`.13`: the `tasks` check requires 2
processors, not N); one shootdown at a time (`.15`); **the bound on a wait
for a processor that never answers (`.17`), proved only by the negative
control in commit 94dee288's message**, a scratch edit that made processor
2 stop answering and ended the boot in FX-0001 after 1.8 s under KVM, 5.1 s
under tcg and 32 s under the coverage plugin, **and the bound on the turn
(`.18`), proved by nothing** (commit 237d2426: making a holder stop with the
turn in hand breaks what would release it); the interrupt re-sent every 10
ms (`.19`); the two shootdown rules, which are debug assertions (`.20`);
`service_tlb`'s whole flush for a processor far behind, and
`scoped_request`'s answer to members and non-members (`.21`, `.22`); a
scoped shootdown with nobody to ask (`.25`); `add_cpus` (`.28`);
`stop_others` and where a processor looks for the stop (`.30`, `.31`); and
the scheduler's kick reaching its one target (`.32`). Every stuck-processor
case needs the same thing, a boot that expects its panic -- as
`test-init-file`'s `ferrix.onexit=panic` boot does -- with a kernel switch
that makes one processor stop answering.

**2026-10-07, FX-0001 under load (stage 20 S-1).** A wait now has a late
and a stuck bound (MEMORY-AND-TIMING.md §2.2a). `L.smp.33`, a processor that
answers past the late bound waited for and counted late, is new and verified
by `late_answer`, appended to `smp/check.rs` (the `late` line, on all four
boots: a grace period everywhere, a shootdown on x86-64). `.17` now names
the stuck bound, 10 s and 167,772,160 polls; 94dee288's control measured the
old 1 s bound, and the control in the FX-0001 commit's message, a processor
that never answers, measures the new one under KVM and `tcg`. `.18` is 40 s
and is still proved by nothing, for the reason 237d2426 gives. `L.smp.34`, the
late answers reported only once the wait has returned and the turn is free,
at a few lines a boot, is written unverified and in the baseline: a boot
that starves a processor shows it, as the starved `test-selfhost --accel kvm
--smp 8` of the FX-0001 commit did (`g3-selfhost.log` in
`~/ferrix-logs/fx0001/2026-10-07-po10/` on nazuna: four lines, then the
summaries at 8 and 16, each after the wait). smp now has 34, 10 verified and 24
that need a check.

What this slice found, for review:

* **Grace periods have no product caller.** `synchronize` and
  `read_section` are called only by stage 4's `grace` check; `irq.rs` names
  the handler unregistration they are for, which nothing does yet. The
  answering half runs on every boot (`on_ipi`, `secondary_main`), so they
  are traced as product code under `H.MEM.19`, not listed as check code;
  whether an unused mechanism belongs in the item is DO-178C's deactivated
  code question for the item's owner.
* **`H.FAIL.1` says the kernel stops every other processor, and its
  criterion does not test it.** `L.smp.30` and `.31` refine that half and
  have no check; the split (`H.FAIL.1` keeping the report, a new one taking
  the stop) is proposed for the failure slice, not made here, since no one
  check proves either half today.
* **`migrating_shootdown` proves `.16` on x86-64 only**: on the Arm pair,
  whose invalidation is broadcast, and with fewer than 3 processors it
  returns without testing, and the matrix will still show it reached there.
  `.16`'s statement says "where invalidation is not broadcast".
* **Stage 4's exit criterion, `contended`, proves `crate::sync::SpinLock`**,
  not smp, and is left untagged for the slice that writes `sync`.
* **Time.** About 40 minutes of agent time from the first read to the
  gates: fifteen reading smp.rs's 1,500 lines and its 850 of checks with the
  commits behind the bounds, fifteen for the 32 requirements and three
  high-level ones, ten for tags, the baseline and the documents.

**Step 4, the x86-64, trap and system-call-entry slice, done 2026-09-27.**
128 low-level requirements in `docs/sysml/18-x86-64-requirements.sysml`, in
three id spaces: `L.x86_64.1` to `.119` for `src/kernel/src/arch/x86_64/`,
`L.trap.1` to `.6` for `src/kernel/src/trap.rs`, and `L.syscall.1` to `.3` for
`syscall/mod.rs`'s dispatcher (`dispatch_with`, `native_call`,
`unanswered`); one package per part -- descriptors, user state, processors,
paranoid entries, speculation, timers, both ABIs' signal frames, the
SYSCALL entry, int $0x80, exceptions, the processor, memory operations,
MSI, the console and dispatch. **57 are verified**, by 38 check functions
newly tagged or given more ids: `arch/x86_64/{gdt,trap,paranoid,syscall}/
check.rs`, `arch/speculation_check.rs`, `syscall/{check,vdso_check}.rs`,
`smp/check.rs`, `sched/check.rs`, `user/check.rs`, `object/check.rs`, and
the xtask gates `test_boot_lines`, `entropy_problem`, `reset_problem`,
`init_file::test` and `jobs::test_jobs`. **71 need a check** and are the
baseline's. Of the slice's 305 product functions, 221 were named by
nothing; each is now a requirement's unit or, ten of them, check code in a
product file (`traceability-units.json` says why: the debug-register and
SMAP-window writes only checks make, the speculation check's machine half,
the #DB hook, `check_exception_entry`, `trap::breakpoint`,
`identity_map_live`). The gate holds `arch::x86_64` and `trap` as complete.
Nothing moved: the checks this slice would have moved are `main.rs`'s, and
moving them is its own change.

The high level grew from 77 to 88, for behaviour no requirement said:
`H.TRAP.8` (an i386 program runs in compatibility mode and enters only
through int $0x80, answered as i386 Linux answers it; `check_compat`),
`H.TRAP.9` (a 64-bit handler's frame is Linux's and its return resumes it;
the `signals` program), `H.TRAP.10` (registers pass the kernel as the ABI
says; no one check), `H.TRAP.11` (an unknown number is ENOSYS) and
`H.TRAP.12` (a number is decoded by the build's own table); `H.SCHED.7`
(thread-local descriptors are the thread's own; `check_thread_areas`),
`H.SCHED.8` (FS and GS bases and x87/SSE state are the task's own; no
check) and `H.SCHED.9` (a vDSO clock answers as the call); `H.BOOT.6` and
`.7` (the run ends powered off, or reset when asked; xtask's gates) and
`.8` (the serial console carries the session; no one check). The per-processor
record's register (`L.x86_64.86`) serves the smp slice's `H.SCHED.6`, which
says it already. `H.TRAP.4` and `H.TRAP.7` are now verified, by
`trap::check::run` and `gdt::check::run`. `H.TRAP.6` is not: its criterion
says each exception runs "on the kernel's own stack", and `paranoid/check.rs`
asserts the count and GS, not the stack, although its `nmi` line prints
"taken on its own stack". 43 of the 88 are verified.

The 71 that need a check, as check-writing work:

* **64-bit signal frames.** No check forges a 64-bit frame (`L.x86_64.43`),
  so on x86-64 **`H.TRAP.5` is proved for i386 frames only**; the i386 check
  misses VIF, VIP, ES and plain `sigreturn` (`.49`). `sanitised` against a
  hostile trap frame (`.44`), the floating-point state through a frame
  (`.45`, `.13`), a handler without `SA_RESTORER` (`.46`), and the i386
  handler's registers, mask and alternate stack (`.50`, `.51`).
* **User state per task.** FS and GS bases (`.9`, `.61`) and x87/SSE state
  (`.10`) across a switch, their inheritance by a fork (`.11`) and their
  reset at execve (`.12`), a stale selector at restore (`.16`), DS and ES on
  an i386 program's first entry (`.14`) -- the i386 fixtures reach memory
  only through SS and GS outside a handler.
* **Entry and registers.** R8 and R9 as arguments (`.56`), the SYSCALL flag
  mask (`.57`, whose TF half `main.rs`'s `check_trap_flag_entry` checks) and
  STAR's RPL (`.58`), a clone child's stack (`.65`), the restart-block paths
  (`.67`), registers zero on a fresh entry (`.70`), execve into an i386
  image from either entry (`.71`, `.75`), `AT_PLATFORM` (`.76`), int $0x80
  in the native range (`L.syscall.2`), and `unanswered`'s bound
  (`L.syscall.3`; `service_check` makes the call and asserts nothing).
* **Descriptors and processors.** The TSS and IST stacks read back per
  processor (`.3`), port access from ring 3 (`.4`), RSP0 after switches
  (`.6`) and at boot (`.104`), a secondary's failed allocation (`.5`), the
  trampoline's frames (`.20`), CR4 on every processor (`.21`, `.90` -- the
  SMEP/SMAP/UMIP `cpu` line is printed for the boot processor and never
  asserted), UMIP from ring 3 (`.112`), a directed IPI and a refused APIC
  ID (`.22`, `.23`, `.93`), the paranoid entries' fatal cases and the
  double fault's stack (`.27`, `.83`), vectors ring 3 may not raise (`.81`),
  a program's int3, single step and #AC (`.82`; AArch64 and ARMv7-A have
  this check and x86-64 does not).
* **Timers and interrupts.** The LAPIC timer's rate and one-shot (`.34`,
  `.35`), whose checks are `main.rs`'s `timer_check` and `check_one_shot`;
  the counter's rate against a reference it was not calibrated from
  (`.38`) -- `timer_check` cannot see a wrong rate, since the timer is
  calibrated from the same counter --, its bring-up failures and
  monotonicity (`.39`, `.40`); the I/O APICs masked and COM1 routed (`.36`,
  `.37`, `.96`); nesting interrupt masks (`.92`); `mask_interrupt` refusing
  (`.94`); the spurious vector (`.95`); MSI vectors exhausted (`.114`).
* **Speculation.** The plan's choice per CPUID (`.32`, a table-driven test)
  and the exposure line (`.33`). The read-back check is tagged, but **under
  TCG the processor offers no controls and nothing is written**: only the
  KVM boot row exercises it, and its criterion says so.
* **Kernel faults and the end of a run.** Kernel breakpoints and demand
  paging (`.84`, `L.trap.3`), whose checks are `main.rs`'s; a fatal trap's
  report (`.85`); a processor staying halted after a panic (`.100`); a
  backtrace line (`.105`); the triple-fault reset (`.106`); the RNG words
  (`.107`); CR0.WP (`.111`, `main.rs`'s `self_check`); one-page
  invalidation (`.109`); returning to the kernel's root (`.89`); the console
  transmit interrupt (`.116`, whose check `console::output::check` is in a
  product file), its FIFO and bounded waits (`.117`, `.118`); no entry
  registered (`L.trap.5`, unreachable in a boot) and a moved process's new
  job (`L.trap.6`).

What this slice found, for review:

* **`main.rs` holds the strongest checks this slice's code has**: stage 3's
  breakpoints and demand paging, the timer's rate and one-shot, the trap
  flag, CR0.WP. Moving them to check files verifies four requirements whole
  (`.34`, `.35`, `.84`, `L.trap.3`) and half of two more (`.57`, `.111`), as
  the iommu and memory slices' moves did theirs.
* **Three console lines print more than their checks assert**: `nmi` ("taken
  on its own stack"), the SMEP/SMAP/UMIP `cpu` line, and `random`.
* **`watch_to_power_off` takes an exit with status 0 after the marker as a
  power-off.** Both it and `test-boot` require the success marker first, and
  status 0 is right for them: ACPI's S5 and a QEMU asked to stop both exit 0.
  But a triple fault under `-no-reboot` after the marker exits 0 by itself as
  well, so a run that faults on its way down passes as powered off
  (`L.x86_64.98`, `H.BOOT.6`); telling them apart needs the kernel's own
  power-off line.
* `check_compat` accepts SIGILL for SYSCALL from compatibility mode, so the
  return from 0x23 to 0x33 is proved only on the -ENOSYS path.
* **Time.** Drafted by five agents in parallel, one per group of files,
  from the pilot's lessons, in about 15 minutes; merging their overlaps,
  the high-level additions, review and tagging took about an hour.
**Step 4, the `arch/aarch64` slice, done 2026-09-27.** 49 low-level
requirements, `L.aarch64.1` to `L.aarch64.49`, in
`docs/sysml/19-aarch64-requirements.sysml`, one package each for traps,
signals, the context switch, interrupts, the timer, the console,
translation, the processors, speculation and firmware. **16 are verified**,
by 13 check functions newly tagged: `arch/aarch64/check.rs`'s machine
check (syndromes and signals, a system call rewound, `console=` values, the
TRNG against scripted firmware, the GIC's driver per description, masking
read back, refusals), `arch/aarch64/trap/check.rs`'s programs, and the
speculation check, and stage 3's breakpoint and timer checks, which the boot
slice's 21a moved into `stages_check.rs`. **33 need a check** and are the baseline's
`L.aarch64.*`. Every product function of `arch::aarch64` is a unit, an
accessor or one of 6 check-code entries (the machine check's entry, the
exception-entry hook, and the GIC's two read-backs in each of `gic.rs` and
`gicv3.rs`, which need register windows private to them); the gate holds
`arch::aarch64` as complete. The PL011 and the GICv2 are `arch_common`'s,
shared with ARMv7-A, and wait for that slice.

What moved: speculation's boot check (`check`, `check_part_tables`,
`check_firmware_answers` and the fixed-size `Line` they format into) left
`speculation.rs` for `speculation/check.rs`, a child module that reads the
part tables and records private to its parent, so that a tag can go on it.

The console slice, which landed first, owns the ramoops value's parse
(`L.console.40`); `L.aarch64.27` is the 16550's alone, and the machine
check that tries both carries both tags.

No high-level requirement was split or added. Nine hang from the ones the
x86-64 slice added, the same promise on this architecture: the per-CPU
record from `H.SCHED.6`, the signal frame from `H.TRAP.9`, the system call
path from `H.TRAP.10` and `.12`, the processors from `H.BOOT.5`, power-off
and reset from `H.BOOT.6` and `.7`, and the console from `H.BOOT.8`. The
TRNG refines `H.BOOT.3`, as the certification session asked. `H.SCHED.8`
(the user state a trap does not save) names only x86-64's registers, so
`.12`, this architecture's FP/SIMD state and thread pointer, stays under
`H.SCHED.1` until it is widened to say each architecture's.

The 34 that need a check, as check-writing work:

* **Asserted only by product code.** Stage 1's identity map (`.32`) and
  stage 4's count of started cores (`.37`) are still in `main.rs`; moving
  them into a check file, as 21a moved stage 3's, would verify both.
* **Exercised by every boot, asserted by nothing here.** The system call
  path (`.4`), the per-CPU record (`.7`), the signal frame's layout (`.9`),
  the context switch and the user FP/SIMD state (`.11`, `.12`), the
  controller's bring-up, GICv3 configuration, claim-and-retire order, IPIs
  and the ITS (`.14`, `.15`, `.18` to `.20`), the MSI doorbell (`.22`), the
  interrupt mask (`.23`), the counter (`.25`, `.26`), the console's bytes
  and input (`.29`, `.30`), user roots and the TLB (`.31`, `.33`),
  processors described (`.36`), hwcaps (`.38`) and the index clamp's
  instruction sequence (`.43`). The generic checks run most of these but
  state the generic layer's criteria, so crediting them here would claim
  what they do not assert.
* **Needs the item to stop, or hardware QEMU lacks.** A kernel trap's report
  (`.5`), sigreturn keeping EL0 (`.10`, which x86-64 has forged frames for
  and AArch64 does not yet), cache maintenance (`.34`, which QEMU does not
  model), PAN refusing a kernel read (`.35`), the store bypass defence read
  back (`.42`, which QEMU's max CPU does not need), the firmware TRNG and
  RNDR themselves (`.46`), power-off and reset (`.47`) and the Pixel 7's
  watchdogs (`.48`), and `dma_barrier`'s `dmb osh` (`.49`, F-44), which no
  run under TCG can show missing. The console's 16550 and ramoops arms (`.28`, `.29`)
  run only on the crosvm guest and the phone, whose logs are cited, not
  checked.

**Step 4, the `console` slice, done 2026-09-27.** 41 low-level
requirements, `L.console.1` to `L.console.41`, in
`docs/sysml/23-console-requirements.sysml`, one package each for the
kernel's lines, a program's output and the transmit ring, failure reports and
the recent-output ring, input, the kernel log and the screen console, and a
`Ports` package for the two Arm functions that choose the console,
`arch::aarch64::console::ramoops_zone` and `arch::armv7a::console::chosen`
(with `forced`, `first_enabled` and `Port`'s two), whose checks test console
behaviour; the rest of each port is its arch slice's. **12 are verified**,
by 7 check functions and 1 xtask gate newly tagged: `console/log_check.rs`'s
`wrap_overrun_and_partial` (`.26` to `.28`), `two_writers` (`.25`, `.29`)
and `console_records` (`.31`, `.32`, `.34`), `service_check.rs`'s
`the_last_line_is_kept_for_a_failure_report` (`.17`),
`arch/aarch64/check.rs`'s `check_ramoops_zones` (`.40`),
`arch/armv7a/check.rs`'s `check_chosen` (`.41`), and xtask's
`init_file::judge_k7_read`, the FX-1501 line on the port after a panic boot
(`.14`); `log_check::run` carries `H.TRAP.13`. **29 need a check** and are
the baseline's `L.console.*`. Of console's 74 product functions 62 are a
requirement's unit (19 of them accessors named as an entry point), 10
accessors and 2 check code -- `console::input::check` and
`console::output::check`, which read their rings' private state; as child
modules (`console/input/check.rs`, as `object/pin/check.rs` was made) they
could be tagged and would verify `.20`, `.9` and `.10`. **0 are named by
none**, and the gate holds `console` as complete. Doc-comment lines only:
nothing moved. The item's functions named by none go from 925 to 874 of
2,346 (console's 45 and the six Arm choice functions).

The high level grew from 88 to 95, by behaviour no requirement said, with no
split. **`H.BOOT.9` is proposed, not only added**: the kernel log is
readable by privileged programs and the Pixel 7's USB driver, and F-31's
KASLR half is worth only as much as nothing they read gives the layout away,
so the log shall hold no slide and no kernel address -- no high-level
requirement said so; parent O.ISOLATE, as `H.BOOT.3`. Its criterion (the
whole log searched for every address the serial log printed) has no check;
`L.console.34`, a line sent unlogged is not in the log, is verified. The
other six: `H.TRAP.13`, a read of a log ring hands only recorded bytes in
order and counts exactly what it skips (T.CONFUSE path 6; verified by
`log_check::run`); `H.TRAP.14`, a program's output reaches the port as
written, and `H.TRAP.15`, the log holds what the console sends -- both under
`G.5`, since the console's program-facing half serves the system-call
surface rather than an objective; `H.SCHED.10`, the console holds a
processor with interrupts masked only for bounded work, and `H.SCHED.11`, a
writer that may sleep sleeps for room and one that may not never does (both
O.QUOTA); and `H.FAIL.4`, a report gets past a lock nobody will release.
The kernel's lines out and a person's bytes in are the x86-64 slice's
`H.BOOT.8`, which the console's port-side requirements refine rather than
repeat. 44 of the 95 are verified. Of the log's other properties,
privileged reading is `syslog(2)`'s and `logctl`'s, outside the item, and
recording with no lock, no allocation and no wake, which is what makes it
safe in a panic, is `L.console.35`, which no check tests.

The 29 that need a check, as check-writing work: nothing sent or recorded
before the port is ready (`.1`); a line whole under concurrent writers and a
poller after the ring (`.2`, `.3`); `write_bytes` against `write_raw` on the
port (`.4`); a writer that may not wait, one that waits for room, the chunk
rule and the log written once (`.5` to `.8`); the burst and the transmit
interrupt, whose check exists in a product file (`.9`, `.10`); the wake at
half, a stalled port, `drain` (`.11` to `.13`); **a report past a lock held
for good, and `drain` bounded in a report (`.15`, `.16`)**, which want a
boot that expects its panic, as smp's stuck-processor cases do; the recent
ring's kernel-only rule and bounds (`.18`, `.19`); the receive ring, whose
check is in a product file, its wake, the handler's bound, `init` and
`read_byte` (`.20` to `.24`); a cursor ahead of the newest byte (`.30`);
a kernel line in the log (`.33`); recording that never waits (`.35`); and
the screen console, none of whose behaviour a check reads (`.36` to `.39`).
`next_chunk` (`.7`) and `Board::new` (`.37`) are pure functions a small
check could cover in minutes.

What this slice found, for review:

* **`service_check::a_write_that_may_not_wait_is_queued` asserts nothing**:
  its doc says the line is the proof, and no gate reads the line, so it
  cannot carry `.5`.
* **`output::check` proves less than its line suggests on QEMU**: an emulated
  port is never full, so the transmit interrupt's share is whatever the
  writer's one burst left; the full-ring and waiting paths are not reached
  on any boot.
* **The smp boot line reported one round where 100 ran** (fixed here):
  `smp::check::contended` wrote its round into `Report::rounds`, which
  `everywhere` had set to 100, so the `smp` line said *1 rounds of work*;
  contended now keeps its own `counter_round`, and x86-64 prints *100 rounds
  of work on every processor* and *shares overlapping in round 1*.
* **Time.** About 75 minutes of agent time: fifteen reading console's 1,900
  lines and the checks that reach it across the tree, twenty for the 41
  requirements and the high-level ones, fifteen for tags, the smp fix and
  its boots, and twenty-five for the documents and a rebase over the x86-64
  slice, which had landed first and taken six of the high-level ids this
  slice had drafted (they are renumbered; no tag had named them) and
  written `H.BOOT.8`, the console's session, which replaced two drafted
  here.

49,431 lines of item product code trace to 33 system-level requirements, and no
test names a requirement id.

**This is not a testing task.** There are 31,135 lines of in-kernel self-test
already, asserting genuinely rich properties. What is missing is requirements
for them to discharge.

1. Write low-level requirements in `docs/sysml/` for the item's modules, with
   stable bracketed ids as `01-requirements.sysml` already uses. Verifiable
   statements with pass/fail criteria — not the narrative rationale the current
   33 are (F-16).
2. Start from [SECURITY-TARGET.md](SECURITY-TARGET.md) §7, which already maps
   eight objectives to code and test. That is the shape; extend to modules.
3. Attach ids to the in-kernel check assertions. They print counted quantities
   already (*"2387 mappings swept, 899 executable, none writable"*); each needs
   the requirement it discharges.
4. Generate a traceability matrix and gate it: fail on a requirement with no
   verification, or a test naming a requirement that does not exist. Same
   pattern as `gen-arch-doc.py --check`.


**Step 4, the `claim` and `device` slice, done 2026-09-27.** 35 low-level
requirements in `docs/sysml/20-device-requirements.sysml`, in three id
spaces: `L.claim.1` to `9` (a device claimed through a core's control
channel, and the numbers nodes are published under), `L.device.1` to `21`
(what a node hands out, the nodes published, the bus mastering a node's DMA
is switched with) and `L.quiesce.1` to `5` (the quiesce in
`syscall/native.rs`, whose other units are the syscall-entry slice's; the
module stays short of complete). **26 are verified**: every `L.claim`,
`L.device.1` to `13` and `L.quiesce.1` to `4`, by the claim checks of
`service_check.rs`, the node checks, `iommu/check.rs`'s domain check, two
refusals of `syscall/native_check.rs`, `block_ring/check.rs` and xtask's
`test-init`; **9 need a check** and are the baseline's. The gate holds
`claim` and `device` as complete, `device::Aperture::whole_pages` named
as a unit of `L.object.43`, which it serves. What changed:

* `check_node`, `check_exclusive` and `check_msix` out of `device.rs` into
  `device/check.rs`, a child module, their bodies byte-identical, and still
  run by `device::publish` on every boot; `check_node` leaves the complexity
  baseline with the item.
* Three checks new: `check_dma_switch` reads a PCI function's command
  register back after `enable_dma` and `disable_dma` (nothing had), so a
  quiesce's DMA off is proved at the function; a claim's wait ends `Ok`
  with its release before the wait or while it is parked -- a task releases
  once the waiter is blocked, not after a sleep -- and answers `Waiting` at
  its patience, which `Claims::wait_within` takes explicitly so the check
  waits 10 ms where products wait five seconds; and a claim or a number
  refused for memory, and a node's failure sentence for each kind of node.
* The high level, for what the claim and the quiesce promise and nothing
  above them stated: `H.DEV.1` (a device claimed no more than its core
  allows: `service_check::run`), `H.DEV.2` (a quiesce refused under a live
  driver) and `H.DEV.4` (a quiesce answers success only once every core has
  let go and the function's bus mastering is off), both verified by the
  block ring's `round`, which now reads the command register back -- on
  once DMA is turned on as a driver's first pin turns it, off after the
  quiesce of its death -- and `H.DEV.3` (a restarted driver's device keeps
  its number), under the design rule `P.2`, which `check-traceability.py`
  now accepts as a parent with the goals, and waiting for a check.
* `device/**` joins the core in `certification-item.json`, as `iommu/**`
  is, and the `DEVICE` failure mode's evidence names
  `device/check.rs::check_exclusive`.

The 9 that need a check: the reserved set's contents (`.14`), a host-visible
window's refusals (`.15`), an MSI-X table not found where it is not
(`.16`), legacy lines taken once each (`.17`), the device tree and board
nodes' refusals (`.18`), a tree node described (`.19`), nothing minted for
an unpublished node (`.20`), a USB host's input functions (`.21`), and a
quiesce's two ends, `TIMED_OUT` and `BAD_STATE` from DMA that cannot be
turned off (`L.quiesce.5`). Most want a machine QEMU does not present: an
STM32MP15 or GS201 board, INTx without MSI, a function whose BAR firmware
left unassigned.

Unblocks DAL C, 62304 §5.4 and `ADV_TDS.3` at once.

### Design (2026-09-27)

**Three levels, one chain.** Each level names its parent, so every low-level
requirement reaches something an assessor already accepts:

| Level | Ids | What it is | Where it comes from |
|---|---|---|---|
| System | `O.*`, `ASR-*`, `G.*` | the Security Target's objectives, the safety manual's assumed safety requirements, the goal | exists: SECURITY-TARGET §4, SAFETY-MANUAL §2, `01-requirements.sysml` |
| High-level | `H.<area>.<n>` | what a subsystem of the item promises at its interface: `H.MEM.3` "a frame is mapped writable in at most one address space unless a VMO both hold shares it" | decomposed from the SFR mapping in SECURITY-TARGET §8.2 and the ASR table, one area per SFR family the item implements (MEM, OBJ, SCHED, IRQ, DMA, TRAP, BOOT, QUOTA) |
| Low-level | `L.<module>.<n>` | what one unit of code does: `L.mm.4` "`unmap_in` returns no page-table frame to the allocator before the shootdown that covers it has completed on every processor it reached" | written per module of the `core` and `item` rings |

**The requirement is a model element, not prose.** High- and low-level
requirements live in the SysML model, one package per area
(`docs/sysml/13-item-requirements.sysml` for the `H.*`, and one file per
subsystem for the `L.*` as they are written), with the ids in angle brackets as
`01-requirements.sysml` already does. Each carries, as attributes the gate
reads:

* `statement` -- one sentence with *shall*, about observable behaviour (F-16);
* `criterion` -- the pass/fail condition a test can check, in counted terms
  where there is a count ("every one of the N mappings swept is not both
  writable and executable");
* `parent` -- one or more ids of the level above (`L.*` → `H.*`, `H.*` → `O.*`,
  `ASR-*` or `G.*`);
* `unit` (low level only) -- `path::function` in `src/kernel/src`, which the gate
  resolves with `tools/common/check/rustlex.py`, so a renamed function breaks the
  trace loudly instead of silently.

The existing parser (`tools/common/gen/sysml/parser.py`) reads these; the generated
architecture document gains a requirements chapter from them.

**Verification is named where the check is.** A check function, in a
`check.rs` or `*_check.rs` file, a host `#[test]`, or an xtask gate, carries
the ids it discharges in a doc line the gate parses:

```rust
/// Verifies: L.mm.4, L.mm.5
fn tables_wait_for_their_shootdown() -> Result<(), &'static str> {
```

A check that proves a refusal names the requirement the refusal enforces;
its negative control (recorded in the commit that added it) is the evidence
the check can fail. The id goes on the *function*, not on each assertion:
1,026 check functions today, against tens of thousands of assertions, and a
function is the unit a boot line reports.

**The chain closes at run time too.** A requirement is verified on the
reference configuration only if (1) a check names it, (2) that check's lines
are reached in the coverage evidence for the architecture (drcov already
records it, F-10), and (3) the boot reaches `FERRIX-BOOT-OK`. The matrix shows
all three per architecture, so a check compiled out on one architecture shows
as unverified there rather than passing everywhere by its name.

**The gate** (`tools/common/check/check-traceability.py`, run by `cargo xtask
check`, generating `docs/certification/TRACEABILITY.md` with a `--check` mode):

* fails on an id named by a check that no requirement defines;
* fails on a low- or high-level requirement with no `parent`, a `parent` that
  does not exist, a `unit` that does not resolve, or no `statement`/`criterion`;
* fails on a requirement with no verifier -- **as a ratchet**: a baseline file,
  `tools/common/data/traceability-baseline.json`, lists the requirements written but
  not yet verified, and it may only shrink, as `fallible-alloc-baseline.json`
  does;
* reports, without failing yet, the item's functions no low-level requirement
  names as its `unit` -- DO-178C's "no unintended function" question, which
  becomes a ratchet of its own once a subsystem is fully written.

**Rollout.**

1. The format, the gate and an empty register (one slice, docs and a script):
   the gate passes with nothing written and fails on a malformed entry, with a
   self-test.
2. The high level, all at once (one slice): perhaps forty `H.*` requirements,
   decomposed from SECURITY-TARGET §8.2 and ASR-1..8. These are few, and
   writing them together keeps the areas disjoint.
3. A pilot subsystem end to end: `object/` -- handles and rights (T.FORGE,
   ASR-3), the best-tested part of the item (135 refusal assertions). Its
   `L.object.*` requirements, and `Verifies:` on its check functions, until its
   part of the baseline is empty. This is where the format is corrected before
   it is copied.
4. The rest by subsystem, each a slice a session can take: `mm` and `user`,
   `sched`, `iommu`, `trap` and the syscall entry, `smp`, each `arch/<isa>`,
   `console`, `claim` and `device`, `boot`. Each slice writes the `L.*`,
   tags the checks, shrinks the baseline, and names any requirement no check
   verifies yet -- those become check-writing work, not argument.

F-15 closes when every item module has low-level requirements and the "no
unintended function" report is empty; F-16 when every requirement has a
`statement` and `criterion` the gate has accepted; F-14 when the baseline of
unverified requirements is empty and the matrix shows each verified on all
three architectures or argued as architecture-specific.

**Where W-8 stands, 2026-09-27 wind-down** (main 4636a412). 112 high-level
and 602 low-level requirements; 396 named by a check, 318 in the baseline,
247 tagged checks. Of the item's 2,308 product functions, 1,141 are a
requirement's unit, 545 accessors and 35 check code, and 587 are named by
none. Complete (the gate fails on an unnamed function): `arch::aarch64`,
`arch::x86_64`, `claim`, `console`, `device`, `early`, `iommu`, `mm`,
`object`, `smp`, `trap`, `user`, `vmap`. Model files 14 to 20, 22 and 23 are
written; 21 (boot) has its first landing, 21a, which moved main.rs's stage
checks into `stages_check.rs`.

*In flight, each with the certification consultant's conditions in
`~/.local/share/ferrix/cert-consultant/reviews.md`:*
- **21b** (ferrix-15's audit/init session): file 21's requirements for the
  crate root, init, power, random and the self-check switch, with new
  `H.BOOT.10` to `.14`, `random::check` moved to `random/check.rs`, and F-51
  closed. `H.BOOT.14` also becomes a parent of `L.aarch64.44` to `.46`. The
  list is agreed; the diff is not yet reviewed.
- **21c**: `devmgr`'s 33 functions as their own package of file 21. Not
  started.
- **24** (ferrix-55b's virtio/seam session): `arch/armv7a` and
  `arch/arm_common`, written after F-48 and F-49 are fixed, with every
  requirement stating the correct behaviour.

*Not started:* the load-facing and remaining core modules the gate does not
yet hold complete (`sched`, `syscall/native.rs`, `audit`, `pci`, `irq`,
`timer`, `panic` and others; `--report` lists the unnamed functions by
module). Proposed and not done: widen `H.SCHED.8` to each architecture's
FP/SIMD state (file 19 left AArch64's under `H.SCHED.1`).

---

## W-9 — Vulnerability analysis against the ST threat model

**Done 2026-09-25.** See F-21a and VULNERABILITY-ANALYSIS.md. It found that
no SMAP, SMEP or PAN was enabled, which F-32 then fixed on x86-64 and AArch64.
The attack tests per threat below remain worth writing as regression tests.
Corrected 2026-09-26: its T.EXHAUST paths credited job quotas that are not
built, and the verdict was *not resisted* but for CPU per task (F-35) until
W-13 built them the same day, and *partially resisted* until W-15 charged
the Linux personality's heap to the job; it is now *resisted* (F-37).

**Closes:** F-21a. **Size:** medium. Last EAL5 gap that is engineering.

[SECURITY-TARGET.md](SECURITY-TARGET.md) §3.2 states seven threats: T.MEMORY,
T.ESCALATE, T.FORGE, T.DMA, T.RESIDUAL, T.EXHAUST, T.CONFUSE. Nothing has
systematically tried to realise them.

Write attack tests, one per threat, as ring-3 programs or in-kernel checks.
T.FORGE is the most tractable start — fabricate and guess handles, confirm
every attempt is refused. T.RESIDUAL has an oracle already: allocate, write a
pattern, free, reallocate, confirm zeroes.

Raw material: the 30 fuzz targets and `syscall/check.rs`'s 9,537 lines of
refusal tests. What is missing is an analysis *structured by threat* with a
documented verdict per attack path.

---

## W-10 — Evaluate Ferrocene

**Closes:** F-17. **Size:** unknown until step 1. Procurement as much as
engineering — **answer step 1 before planning anything that depends on it.**

1. Which `rustc` versions does Ferrocene ship, and does its qualified target
   list cover `armv7a-none-eabi` and the three UEFI targets? Expect those four
   to fall outside it.
2. If they do, the reference configuration in
   `tools/common/data/certification-item.json` must say which targets are built with a
   qualified toolchain and which are not.
3. Pinning a Ferrocene release means editing `rust-toolchain.toml`, which is
   its own commit by house convention, and re-running every gate.

---

## W-11 — Side-channel defences, and layout randomisation

**Done 2026-09-26: both halves.** KASLR followed the side-channel defences the
same day: the loader moves the kernel image, the direct map and the vmap
arena's top each boot from `EFI_RNG_PROTOCOL` (18/16/17 bits on the 64-bit
pair, 11/8/9 on ARMv7-A), the 64-bit kernels are static PIEs and the ARMv7-A
kernel keeps its relocations (`--emit-relocs`), stage 1 checks the move, and
`cargo xtask test-kaslr` requires two boots to get two layouts
(SPECULATION.md §6.1). F-31 is closed. See F-31 and
[SPECULATION.md](SPECULATION.md). One build switch, `cargo xtask --mitigations
on|off`, `on` the default and the reference; `on` clamps every program-chosen
index at the system call boundary and applies each processor's speculation
controls, per architecture, read back on every processor at boot (FX-0307).
`cargo xtask check` builds the kernel both ways. Measured under KVM: +1.0% on
system calls, +2.8% on fork-exec-wait.

**Closes:** F-31, with the steps below. **Size:** large for KASLR, small for
each of the rest.

1. **KASLR — done.** As planned, with two departures. ARMv7-A could not be a
   PIE, since its prebuilt `core` uses `movw`/`movt`, so it is a fixed link
   with `--emit-relocs` and a 64 KiB step. And x86-64 needed UMIP, or `SIDT`
   reads the image's slide. Left open: moving the image in physical memory too,
   and the Pixel 7 loader, which keeps the fixed layout and says so.
2. **KPTI**, only if a Meltdown-affected processor enters the reference
   configuration: SPECULATION.md §6 lists the four pieces. Today AoU-11 excludes
   such a processor and the boot log names it.
3. **IBT and shadow stacks**: argue them out, or wait for stable compiler
   support; `-Z cf-protection` is nightly-only.
4. **The residuals in SPECULATION.md §9**: `csdb` for the libraries' clamps
   (needs the clamp to be the kernel's, reached through a trait the tables
   take), the tables deeper than the system call boundary, and a written
   position on cache partitioning.
5. **The direct map's alias of the text — done 2026-09-26 (F-34).** Found by
   step 1: the direct map aliased the image's text and read-only data
   writable, which W^X could not see. Both loaders now map that span read
   only, and every boot sweeps each mapping of its frames (`sealed` line,
   FX-0204). Every interface that maps a physical address a caller names
   refuses a range touching the image (`mm::overlaps_image`), checked each
   boot at stages 1, 2 and 6.

---

## W-12 — Fallible allocation in the item

**Done 2026-09-26.** F-23 is closed. Every allocation in the item's product
code reports failure, except 73 at bring-up that are fatal by design. The
design is [MEMORY-AND-TIMING.md](MEMORY-AND-TIMING.md) §1, and what follows is
what to know before changing code under it.

**The rule the gate enforces.** `tools/common/check/check-fallible-alloc.py` fails
`cargo xtask check` on any call to an allocating standard-library API in the
item that is not argued at the site. So in the item:

* `Box::new`, `Vec::push`, `collect`, `format!`, `to_vec` and the rest go
  through `crate::fallible` (`try_box`, `try_push`, `try_collect`,
  `try_format`, `try_to_vec`, …), re-exported from `src/lib/kernel/fallible`.
* `Arc::new` is `fallible::try_arc`, `Arc::new_cyclic` is `try_arc_cyclic`,
  and a map or set insert is `fallible::insert` or `insert_into_set`. When the
  value must not be lost if the insert is refused, enter the section first
  with `fallible::reserve()` and use `insert_held` inside it. A section masks
  interrupts: hold it across nothing that waits.
* A push into room reserved fallibly just before says so, with `NOALLOC:` on
  the line or in the comment block above. A first-party method named like a
  standard one (the handle table's `insert`, the map's `reserve`) gets
  `FALLIBLE:`. `FATAL-ALLOC:` is for bring-up only.
* On a path that cannot fail -- a drop, a decommit, a close -- get the room
  before the first change. If there is no room, keep what you hold and count
  it; do not allocate.

**Failure injection.** `fallible::inject(task, period)` fails every
`period`th fallible allocation of one task until `stop_injecting()`. It is
what `object/alloc_check.rs` drives, and the quickest way to test a new path.
Room already reserved is never failed by it.

**What is left, and where it is filed.** No bound on the heap as a whole;
per job, since W-13 and W-15, a quota on the heap a job's programs make the
kernel hold (V-05, low). The load's allocations are infallible (AoU-5): converting a
load module the same way is mechanical, but it is outside the item. The gate
cannot see `.clone()`, conversions, or allocation in a callee; the 18 clones
were audited by hand, and the libraries on the item's paths were converted
with it.

**The hole, found and fixed the same day.** `process_create` and
`process_start` make a POSIX process and its first thread in the load, and
`Signals::default` there allocated with `vec!`: a refused frame stopped the
kernel on an item call. The signal tables are now fallible, and the gate
reads the three load files those calls lean on (`REACHED` in the script),
finds a `Default` that allocates in any kernel file and flags a call of it,
and flags a derived `Clone` over an owned heap field. `process.rs` has 14
sites left, recorded in the baseline; the rest of the load those calls reach
is MEMORY-AND-TIMING.md §1.3's table. When the item comes to lean on another
load file, add it to `REACHED`.

**Verify:** `cargo xtask check` (the "fallible allocation" step), and the
`no-mem` line of any boot, which reads the same on every architecture.

---

## W-13 — Job quotas (FRU_RSA.1)

**Done 2026-09-26.** F-35 is closed; V-05 is narrowed to F-37, the Linux
personality's heap. **Chosen 2026-09-26:** build the quotas, not withdraw the
claim. What follows is the design as it was argued before the code, and then
what was built and where it differs.

`object/job.rs` bounds the job tree's depth and descendants and nothing else.
`docs/CGROUPS.md` plans P1 (`pids`), M1 (`memory`) and S1 (`cpu.weight`) as
cgroupfs controllers. FRU_RSA.1 is a claim about the *job*, in the core, so
the charging goes in the core and cgroupfs is one view of it, as it is of the
tree; a native supervisor sets the same limits through a job handle.

### The counters: one quota slot per job, in a table of atomics

Every job but the tree's root gets a **slot** (`src/kernel/src/object/quota.rs`):
for each resource a use count, a limit and a count of refusals, plus the
CPU weight and load, all atomics. A slot names its parent's by index.

* **Why a table and not a field of `Job`.** A frame is freed under whatever
  lock its last holder had -- a VMO's pages lock, an address space's, a page
  table walk -- and has to find its charge there. An `Arc<Job>` cannot be
  dropped under those locks (a drop frees memory and may be a job's last), and
  a pointer needs `unsafe`. A `u32` index into a table that is never freed
  needs neither, and fits the frame record's link field, which an allocated
  frame does not use. The table grows by chunks of 256 slots behind `Once`,
  on demand, from process context; nothing is ever taken out of it, so an
  index read anywhere stays valid.
* **Hierarchical and exact.** A charge of *n* walks from the job to the top
  of its tree, and at each level adds *n* only if the level's use stays at or
  under its limit (a compare-and-swap loop, so two charges racing for the last
  unit cannot both win). A refusal at any level takes back what the levels
  below it took, and counts a refusal there. An uncharge walks the same path
  and subtracts. So a child's use is in every ancestor's count, a limit
  anywhere above refuses, and use never exceeds a limit even for an instant.
* **The root is not charged.** The tree's root has no slot, and a process in
  it charges nothing: the default configuration pays one load of a word on
  each path, and no shared cache line is written by every processor's page
  faults. A limit is only ever below the root, as on Linux.
* **A slot outlives its job for as long as anything is charged to it.** It
  counts holds: its job, each frame tagged with it, each object token, each
  child slot, each task that names it for the scheduler. When the last goes,
  it is free for reuse and lets go of its parent. A frame charged to a job
  that has since gone still uncharges exactly the levels it charged, because
  the chain of parents is kept with the slots. Nothing is reparented, and
  there is no zombie job: only its counters stay.

### What each resource is, and where it is charged

**Tasks** (`pids`, as Linux counts them): a process and each thread beside its
first. Charged in the core's `Process::new` before the process is counted in
its job, and by a thread's id allocation (`registry::allocate_thread`); let go
at `Drop for Process` and at a thread's release. A process moved to another
job takes its task count with it, without a limit check, as Linux's
`pids_can_attach` does. Refused: `EAGAIN` from `fork` and `clone`, as Linux
answers, and `SHOULD_WAIT` from native `process_create`.

**Memory** (`memory`, in pages): every frame a program's memory is built of,
charged to the job of the task that caused it -- Linux's first-touch rule --
and uncharged when the frame goes back to the allocator, wherever that is.
Charged: a fault's commit of an anonymous or file page, a copy-on-write copy,
`fork`'s copy of a held page, a `write` or native `vmo_write` that commits,
the page cache's fill from a disk, and the page tables `map_in` builds for a
user space. The frame record keeps the slot index, so the uncharge needs no
lookup and no lock: it is in `mm::release_frame` and `mm::deallocate_frames`,
under every free path at once. Charges do not move with a process (cgroup
v2's rule). A frame shared by `fork` is charged once, to whoever allocated it.
Refused: the allocation fails as if memory had run out, which F-23 made an
answer everywhere -- `ENOMEM` from a call, the fault's signal from a fault,
`NO_MEMORY` natively. *Not charged*, and argued: the kernel heap (V-05's
residual; bounded per job below), kernel stacks (one per task, so bounded by
the task limit), IOMMU tables and device memory (a driver's, from a device
handle only a driver holds).

**Kernel objects**: the objects the native ABI names and a program can
multiply without a handle to show for it -- a VMO, each end of a channel, a
port, a job, a pin. Charged to the running task's job when the object is
made, held by a token inside it, and uncharged when the object is dropped,
however long after and wherever it went (a channel end parked inside another
channel's queue is still counted). The handle limit alone does not bound
them: a chain of channels, each holding the last one's end in its queue,
keeps any number alive with one handle. Refused: `NO_MEMORY` and `ENOMEM`,
as Linux answers a kernel-memory charge. With the task limit and the per
process limits already there (4,096 handles, `RLIMIT_NOFILE`), this bounds
the heap a job's native objects hold. A page-cache object is the file's, not
a program's, and is not charged as an object; its pages are charged as memory.

**CPU**: a weight per job (`cpu.weight`, 1 to 10,000, default 100), so that a
job's share no longer grows with its runnable tasks. Not a group entity in
`src/lib/kernel/sched`'s EEVDF -- S1's 13 points, the largest change to the scheduler
since EEVDF -- but the same arithmetic done on each task's weight: a job's
*load* is the sum of its runnable tasks' weights and of its busy children's
weights, and a task's effective weight is its own weight times, at each level
from its job up to the root's child, that job's weight over that job's load.
A task in the root job keeps its weight exactly, so nothing changes until a
job exists; *n* runnable tasks in one job share one task's weight. That is
Linux's own approximation of a group's per-processor share
(`calc_group_shares`: the group's weight times this processor's part of its
load), without the per-processor refinement. The load is kept as a task
becomes runnable and stops (one atomic add, and a walk up only when a job
turns busy or idle); the weight is recomputed at enqueue and at each tick of
the running task. A task follows its process to a new job at its next trap or
system call. What it is not: a bandwidth cap (`cpu.max`, S2), and a bound on
the time a job's tasks spend in the kernel beyond EEVDF's own.

### Interfaces

* Native: `job_set_limit(job, resource, value)` and `job_get_quota(job,
  resource, out)`, needing `MANAGE` and `WAIT`; resources memory (bytes),
  objects, tasks and CPU weight. A limit binds the job and everything under
  it, so a supervisor bounds an untrusted program by a job *above* any it
  hands the program.
* cgroupfs: `BUILT` holds `cpu`, `memory` and `pids`. Each child cgroup whose
  parent enables them has `pids.max`, `pids.current`, `pids.events`,
  `memory.max`, `memory.current`, `memory.events` and `cpu.weight`, over the
  same slot. `memory` is a domain controller, so the no-internal-process rule
  becomes reachable.

### Evidence

Boot checks, under a `quota` line on every architecture: each limit refuses at
exactly its value; a parent's limit refuses a child's charge; a job filled to
its limits and emptied reads zero everywhere and frees its slot; a fork bomb
in a limited job is refused at its limit while a sibling can still make
processes; a memory hog in a limited job is refused while a sibling job keeps
committing; eight spinning tasks in one job and one in another share a
processor about evenly. Each with a negative control. A `test-vfs` command
sets `pids.max` to 10 and forks until refused. Cost: page fault, fork and a
null system call timed under KVM before and after, in the root job and in a
limited one.

### As built

Four commits on `cert-f35-quotas`: the design, the charging (core), the
cgroupfs view, and these documents. Where the code differs from the design
above:

* **Tasks are uncharged at reap**, not at `Drop for Process`: the reaped
  process's last reference may be a task the scheduler has not freed yet, and
  a shell running short commands under `pids.max` saw exited ones still
  counted. The parent's `reap_child` and `disown` call `uncharge_tasks`, and
  the drop takes back whatever is left.
* **A native `process_create` loads as a task of the target job**
  (`sched::set_current_group` around the load), so the child's first memory
  is its job's; `CLONE_INTO_CGROUP` does the same around `fork`'s copy. The
  move of a new native process into its job is checked against the task
  limit (`Process::move_new_to`); a move by `cgroup.procs` is not.
* **Objects** are charged by `Vmo::new_anonymous` and `Vmo::fork`, so each
  anonymous or private mapping a Linux program makes counts as one; a page
  cache object is the file's and is not. cgroupfs has no file for the object
  limit, which only a job handle sets.
* **A task follows its process to a new job** at its next trap from user
  mode or system call (`sched::regroup_current`): one per-processor word is
  compared with a global count of moves, 5.3 ns a call under KVM. A process
  that moves itself regroups before its call returns.
* **The effective weight** takes a job's load to be at least what the level
  below adds, so a task not yet counted is never scaled up; without that the
  first check spun one task at 16 million and starved the rest. A job's load
  and what it adds to its parent's are kept by atomics and can drift when a
  job turns busy and idle on two processors at once: the drift scales every
  sibling of that job's parent alike, so shares within the parent hold.
* **A task's state and its job's load change as one step** on its
  processor: `Task::set_state` masks interrupts across the swap and the
  join or leave, and `sched::exit` keeps them masked until it has dropped
  its own reference to the task. A switch acts on the state alone, so one
  between the two found an exiting task dead and still counted, and never
  ran it again: its weight stayed in the job's load, and its `Arc` on the
  freed stack kept the job's slot (FX-0905, fixed 2026-09-26).
* **Page tables** are charged in `map_in` for user mappings only, through a
  `PhysMem` that tags the table with the running task's slot; their frees,
  F-36's deferred ones included, uncharge in `deallocate_frames` untouched.

Measured under KVM, x86-64, best of five, three runs each against `main`
without the quotas: a fault 848 ns against 832, the same in a job two levels
deep; a fork of 256 resident pages 79 µs against 77; within the runs' spread.

Negative controls, scratch, each stopping the boot by its own message: the
limit ignored in `quota::charge` (*"forks went past pids.max"*, the `cgroups`
check); `release_frame` uncharging nothing (*"address spaces gone and their
frames still charged"*); the job share taken out of `effective_weight` (*"a
job with many spinning tasks took more than its share from another job"*,
one task at 111 per mille); `Process::new` charging nothing (*"forks went
past pids.max"*).

**Verify:** the `quota` and `cgroups` lines of any boot, `test-vfs` command
19, and `cat /sys/fs/cgroup/cgroup.controllers` listing `cpu memory pids`.

**What is left:** F-37, the heap the Linux personality allocates for a job,
done the same day as W-15; `cpu.max` (S2), a bandwidth cap, which the ST no longer claims; `memory`'s
reclaim and scoped OOM kill (M1's rest and M2 in `docs/CGROUPS.md`), without
which a job at `memory.max` is refused rather than reclaimed from.

---

## W-14 — Page tables go back after their shootdown

**Done 2026-09-26.** Closes F-36, found by the memory coverage work.

A user unmap freed each page table it emptied at once, before the shootdown,
while another processor could still walk through it from its paging-structure
or walk caches; IOMMU unmaps did the same before the unit's invalidation.
`mm::unmap_in` now puts the tables on the shootdown's `TlbPages`
(`mm/unlinked.rs`, a list linked through the tables themselves, needing no
memory), and `smp::flush_tlb_pages` gives them back after the last answer.
An IOMMU caller releases its list after the unit's flush.

**The rule for new code.** A tree some processor or unit may have walked is
unmapped with `mm::unmap_in` into the `TlbPages` its shootdown will flush, or
with `unmap_io` into a list released after the unit's flush. `unmap_unwalked`
is only for a tree nothing ever walked or everything left with a full flush:
a dropped space, a bring-up tree. A `TlbPages` is not `Copy`: merge with
`add_all`, which moves the tables.

**Verify:** stage 4's check (`tables_wait_for_their_shootdown`), and its
negative control: put `unmap_in`'s old callback back (scratch), and stage 4
must stop at *"an unmap gave back the tables it emptied before its
shootdown"*.

## W-15 — The Linux personality's heap, charged to the job

**Done 2026-09-26.** F-37 is closed; T.EXHAUST is resisted and V-05 is low.
What follows is the design as argued before the code, and then what was
built and where it differs.

W-13 charges a job for its programs' frames and page tables, their native
objects and their tasks. What it leaves out is the kernel heap a program
drives through the Linux personality and the libraries under it: an open
file, a tmpfs inode, a pipe's buffer, a region of its address space, a
message in a socket's queue. Each is bounded by the machine's memory and by
nothing that belongs to one job, so one job can take that heap from every
other, and the load's allocations -- still infallible, AoU-5 -- stop the
machine when it is gone (V-05).

### The audit

Every allocation in the load ring and its libraries that a program can make
*and keep* after its call returns, with the count in the program's hands,
was listed on 2026-09-26 by reading each path from the system call down.
Transient allocations freed before the call returns do not accumulate and
are not listed. Grouped by what is held:

| Kind | Where | Bound before this |
|---|---|---|
| Open file descriptions | `src/lib/fs/vfs` `OpenFile::new`, `with_io` | descriptors per process -- and none in flight, in a mapping, or behind an epoll registration |
| Dentries, anonymous-file locations, mounts | `src/lib/fs/vfs` `Dentry::new`, `Location::detached`, `Namespace::mount` | a 4,096-entry cache, plus whatever an open file or a working directory pins |
| tmpfs inodes, names, symbolic links, instances; a file's VMO | `src/lib/fs/vfs/src/tmpfs.rs`, `fs/pages.rs` | none: `/tmp` and `/dev/shm` are mode 1777 |
| Pipes and their buffers | `fs/pipe.rs`, `src/lib/fs/vfs/src/pipe.rs` | 64 KiB a pipe |
| `AF_UNIX` sockets, their queues, descriptors in flight | `fs/socket.rs`, `src/lib/fs/vfs/src/socket.rs` | 212,992 bytes of payload a direction, but an empty record counts one byte and holds a hundred, and a message carrying 253 descriptors counts one |
| epoll sets and registrations; eventfd, timerfd, signalfd | `fs/epoll.rs`, `fs/eventfd.rs`, `fs/timerfd.rs`, `fs/signalfd.rs` | descriptors -- but a closed file's registration stays until the next wait |
| Regions of an address space; a shared file mapping's records | `src/lib/kernel/vma`, `user/space.rs` | the address space: 2^35 pages. No `max_map_count`, and a shared file mapping makes no VMO for the object limit to see |
| Record and whole-file locks | `syscall/flock.rs` | none: one owner may lock any number of disjoint ranges |
| Descriptor tables | `src/lib/fs/vfs/src/fd.rs` | `RLIMIT_NOFILE` a process |
| A process's recorded program and arguments | `syscall/process.rs` `record_exec` | 256 KiB a process |
| `/proc` and cgroupfs snapshots | `fs/procfs.rs` | one a descriptor, sized by what it shows |
| Internet sockets and their queues | `net/socket.rs`, `src/lib/network/net`, `src/lib/network/nettcp` | 64 KiB each way a connection, 212,992 bytes of payload a datagram socket -- but an empty datagram counts nothing |
| Netlink queues | `net/netlink` | 256 KiB a socket |

And five that are not a missing charge but a leak or a missing check, which
no charge would fix: a closed TCP listener leaks the connections it had not
accepted, with their receive buffers; a process's list of tasks is never
pruned, so a loop of threads grows it for the process's life; netlink adds
addresses and routes to the global tables with no privilege check; an empty
datagram is queued without counting against its socket's capacity; and a
btrfs root's metadata changes are held in memory for up to the commit
interval without counting toward the commit threshold.

### The choice: bytes, charged at the site, to memory

(a) A kernel-memory counter charged at each site, folded into the job's
memory limit as Linux folds `kmem` into `memory.max`; or (b) a count limit
per kind. (b) is simpler at each site and wrong in aggregate: twelve limits
that each allow a job its share still let it take twelve shares, and a job's
supervisor has no single number to set. (a) is what cgroup v2 does, and what
a Linux program expects `memory.max` to mean: `memory.current` counts kernel
memory, and a charge past `memory.max` fails the allocation with `ENOMEM`.
**Chosen: (a)**, with (b) only where bytes cannot be attributed to a job --
the global tables netlink writes, which get the privilege check Linux has.

* **One counter, in bytes.** The memory resource W-13 counts in pages is
  counted in bytes, a frame charging 4,096, so heap and frames meet one
  limit exactly and the compare-and-swap argument holds unchanged. A second
  count, kernel bytes alone, is kept beside it for `memory.stat`'s `kernel`
  line, and never limited.
* **The token.** A new crate, `src/lib/kernel/kmem`, holds a `Charge`: a job's slot
  and a byte count, which uncharges as it drops. The object it pays for
  holds it, so every path that frees the object frees the charge, as W-13's
  object tokens do. It is a crate and not a kernel type because half the
  sites are in libraries (`ferrix-vfs`, `ferrix-vma`, `ferrix-net`) that
  cannot name the kernel. The kernel installs the account it calls through
  at boot; a library's host tests install a recording one; with none, a
  charge is to nobody, which is also what the root job's programs get.
* **What a charge is worth.** What the heap gave, not what was asked: the
  size class a request is served from, or the pages of a large one, from
  `src/lib/kernel/heap`'s own arithmetic. A buffer is charged at its capacity, not its
  length, since that is what it holds.
* **Who pays.** The job of the task whose call made the object, as Linux's
  `GFP_KERNEL_ACCOUNT` charges `current`'s memory cgroup. A buffer that
  grows later is charged where its object is: a pipe's or socket queue's
  growth to the job that made the pipe or the socket, as Linux charges a
  socket's buffers to the cgroup its socket was made in; a message's bytes
  to its writer; a connection a listener accepts, to the listener's job.
* **An object outlives its job, or moves to another.** It stays charged to
  the job that made it until it goes, as a frame does and as Linux's
  `obj_cgroup` does: the charge holds the job's slot, so a gone job's
  counters stay exact until the last thing charged to it is freed. A
  descriptor passed over a Unix socket (`SCM_RIGHTS`) stays its opener's;
  the message that carries it -- the list of files and the queue entry -- is
  the sender's, which is what Linux charges (`scm_fp_dup` is
  `GFP_KERNEL_ACCOUNT` in the sender).
* **Refused is `ENOMEM`**, from the call that would have made or grown the
  object, with nothing changed: a charge is made before the first mutation,
  so a rename refused its new name keeps its old one. A write that has
  queued some bytes reports those. The network's input path cannot answer
  anyone: a segment or datagram whose charge is refused is dropped, as one
  arriving at a full buffer is, and TCP's retransmission makes that
  back-pressure.

### What is argued rather than charged

* **Per-page bookkeeping of charged frames**: a VMO's page list entry is a
  few dozen bytes per 4,096-byte frame already charged, so it is bounded by
  the memory limit at under one per cent.
* **Futex waiters, signal state, a thread's kernel stack and queue nodes**:
  one per task, bounded by the task limit.
* **Pseudoterminals**: 256 pairs on the machine, and a few kilobytes each.
* **The dentry cache** keeps up to 4,096 dentries nobody holds, charged to
  whoever looked them up. A job whose limit they take meets `ENOMEM` where
  Linux would reclaim them; reclaim is M1's rest (`docs/CGROUPS.md`). Its
  queue is made whole with the first namespace at boot (4,096 entries: 32 KiB
  on 64-bit, 16 KiB on armv7a), is shared by every copy, and never grows, so
  no lookup allocates for it.
* **The load's own infallible allocations** stay infallible (AoU-5). What
  changes is that a limited job cannot drive the heap to exhaustion through
  them.

### Evidence

A boot check per kind: a job at a memory limit is refused one more of each
-- an open file, a tmpfs file, a name, a pipe and its buffer, a socket and
a message, a descriptor in flight, an epoll registration, a region, a lock
range -- while a sibling makes the same; and when the job's objects go, its
counter reads zero and its slot is given back. A `test-vfs` command fills
`/tmp` from a shell in a cgroup with a small `memory.max` until creation is
refused, reads `memory.current` and `memory.stat` against it, removes what
it made and sees the charge go. Negative controls in scratch. Cost timed
under KVM: open and close, a pipe write and read, a tmpfs create and write.

### As built

Eight commits on `cert-f37-heap-quota`: this design; `src/lib/kernel/kmem`; the five
fixes the audit found, in four commits -- a btrfs transaction committed once
its changed nodes pass the threshold, a process's task list pruned, netlink
changes refused without privilege, a closed listener's connections reaped
and its handshakes bounded (the empty datagram's accounting went in with
the charging, whose code it shares); the charging and its checks; and the
record-lock kind of the boot check. Where it differs from the design:

* **The account is installed as the first job is made**, not at a point in
  bring-up: until a job below the root exists there is no slot a charge
  could go to, and the one line in `main.rs` would have made the scheduler's
  bring-up function longer than the complexity ratchet allows.
* **A buffer's growth is charged to its object's job, but a socket queue's
  places are charged to the writer**, with each segment: the queue is one
  allocation the maker would otherwise pay for, so a writer in another job
  could fill it at the maker's expense. Each segment pays for four places,
  and a queue under a quarter full gives most of its room back, so it never
  holds more than that beside a floor of four the socket pays for. An
  object's list of mappers does the same, for the same reason.
* **A new process's first descriptors are charged to nobody**
  (`quota::charging_nobody`): `fd::standard_streams` stops the kernel if it
  cannot make them, and a job at its limit must not be able to cause that.
  The table is charged, room and all, to the first job that grows it.
* **The kernel's tmpfs instances** -- the root, `/tmp` and `/dev/shm` as
  boot mounts them, the memfd filesystem -- are `Tmpfs::for_kernel`, charged
  to nobody, since whoever touches one first would otherwise pay for it
  forever. `mount -t tmpfs` charges its instance to the mounter.
* **Objects the network stack makes for a job** carry the job's slot, not
  the running task: a TCP connection a listener accepts is charged to the
  listener's job in the input path, and what arrives for a job at its limit
  is dropped as at a full queue. A stream write that its queue cannot grow
  for is `ENOBUFS`, the stack's existing answer for no memory, rather than
  a wait that could not end.
* **Descriptor tables** were not in the design's first list; `dup2` far
  below `RLIMIT_NOFILE` grew one to there in one call, so they are charged.
  So are `/proc`, sysfs and cgroupfs snapshots, one per open, and a file's
  page store, which the object limit does not count for a tmpfs file.
* **Record locks** are refused `ENOLCK`, as Linux refuses a lock it has no
  memory for, rather than `ENOMEM`.

Measured under KVM, x86-64, best of seven within a boot, three boots each
alternated with `main` (d8cae5a5), a static C program as init, in the root
cgroup and in a child with `memory.max` 1 GiB: an open and close 4,104 ns
against 4,044 in the root (1.5%) and 4,139 against 4,003 in the child
(3.4%); a 64-byte pipe write and read 1,860 against 1,872 and 1,881 against
1,856, within the spread; a tmpfs create, 4 KiB write, close and unlink
12.7 µs against 12.2 (4.0%) and 13.9 against 13.2 (4.8%). The root's cost is
a look at the running task's job at each site; a limited job's is the
compare-and-swap up its tree.

Negative controls, scratch, each stopping the boot by its own message:
`Charge`'s drop uncharging nothing (the `quota` check: *"address spaces gone
and the heap of their regions still charged"*); `quota::charge_kernel`
ignoring the limit (*"kmem: a job made more than its limit could hold"*, on
the files, after 100,000); the kernel's account releasing no slot (*"the
checks' jobs are gone and their quota slots are not"*); `Pipe::new`
forgetting its charge (*"kmem: objects gone and their heap still charged to
their job"*, pipes, 7,168 bytes). The first and third are caught by the
`quota` line before the `kmem` line runs, which is the order they run in.

**Verify:** the `kmem` line of any boot, `test-vfs` command 21, and
`memory.stat` in any cgroup.

**What is left:** reclaim (M2 in `docs/CGROUPS.md`): a job at its limit is
refused, including for dentries its lookups left in the cache, where Linux
would reclaim them. The argued kinds in FINDINGS.md F-37 stay as argued.

---

## W-16 — Trace every `unsafe` in the item to what it serves (F-26)

**Done 2026-09-27.** F-26 is closed: all 663 sites carry an id and the
baseline is empty. What follows is the design, and what to know before
adding an unsafe site to the item: give it the id of what its own operation
discharges, and if none of the fourteen fits, look at the site first.

Every unsafe site already says why it is sound. What an assessor cannot do
with that prose is go the other way: from ASR-1 to every block whose soundness
ASR-1 rests on. This order gives each unsafe block, `unsafe impl` and `unsafe
fn` in the `core` and `item` rings -- self-tests included, since they run in
the same image -- an **obligation id**, and each id a place in the safety
argument.

### The set, and how it was found

Surveyed first, named second. On 9f369ebb the item held 663 sites -- 523
blocks, 20 impls and 120 `unsafe fn`s that need a `# Safety` section (the two
`GlobalAlloc` methods implement the trait's contract and are exempt, as
before). Sorted by what each one actually does, they fall into fourteen
obligations, which are the table in
[SAFETY-MANUAL.md](SAFETY-MANUAL.md) §2 and `unsafe_obligations` in
`tools/common/data/safety-requirements.json`: `TRANSLATE`, `PROTECT`,
`USER-COPY`, `FRAME`, `DMA`, `DEVICE`, `CONTEXT`, `ENTRY`, `SYSREG`,
`FIRMWARE`, `SHARED`, `KMEM`, `BOOT-DATA`, `PROBE`. Each names the ASR, FM or
AoU it serves and the code that argues it. As tagged: 123 `CONTEXT`, 118
`SYSREG`, 87 `SHARED`, 66 `ENTRY`, 60 `TRANSLATE`, 55 `DEVICE`, 29
`FIRMWARE`, 29 `PROTECT`, 28 `KMEM`, 24 `FRAME`, 22 `PROBE`, 11 `DMA`, 7
`BOOT-DATA`, 4 `USER-COPY` -- the gate prints the same table on every run.

Two candidates from the brief did not survive the survey. *Inline assembly
per architecture* is not a reason, it is a means: the item's `asm!` blocks
write a translation root, a vector base, a speculation control or a timer,
and each is filed under what it writes. *FFI and linker symbols* are the
same: `ferrix_switch` is `CONTEXT`, `ferrix_vdso_len` is `BOOT-DATA`. An id
that says *how* instead of *why* traces to no requirement.

The classification rule for a site that could be two things: the id is the
obligation **its own operation** discharges, not the one of the function it
sits in. `write_msr` is `SYSREG` as a primitive; its call writing
`IA32_SPEC_CTRL` is `PROTECT`, its call writing `IA32_LSTAR` is `ENTRY`, and
its call writing `IA32_FS_BASE` for a program is `CONTEXT`. A block inside an
`unsafe fn` that only discharges the function's own contract (`write_cr3`'s
`mov cr3`) takes the function's id. A block that reaches memory through a
`Sync` claim or an `UnsafeCell` is `SHARED` whatever the memory is, because
the exclusion argument is what could be wrong.

### The comment form

`// SAFETY: (DEVICE) <the prose, as it was>` on a block or an impl, and on
an `unsafe fn` the id opens the first line of its `# Safety` section:
`/// (TRANSLATE) The caller guarantees ...`. Not the tidier
`// SAFETY(DEVICE):` -- clippy's `undocumented_unsafe_blocks`, which the
workspace denies, looks for the text `SAFETY:` and refused a block commented
that way when it was tried (rustc 1.97.1). One site may carry two ids,
`(FRAME, DMA)`, when its one operation meets both; none does today.

Putting the id on the existing line keeps every edit comment-only and moves
no line, so the coverage evidence, which is anchored by file and line, stays
valid without carrying.

### The gate

`tools/common/check/check-unsafe-audit.py` finds the comment that covers each site
(it already did, to require one), reads the id, and:

* fails on an id the registry does not define, anywhere in the tree;
* in the item, counts sites with no id per file against
  `tools/common/data/unsafe-trace-baseline.json`, a ratchet like the fallible
  allocation one: a new untagged site fails, and so does a count that has
  fallen without `--record`. The target is an empty map;
* prints the item's sites per id, so a new `(SHARED)` shows in a diff.

`check-safety-requirements.py` holds the manual's table and the register to
each other, requires every `serves` to be a registered ASR, FM or AoU, and
resolves every `argued_by`.

`--report` lists the untagged sites by file.

### Verify

`cargo xtask check` (the "unsafe audit" and "safety requirements" steps). The
tagging commits are comment-only; `git diff main -- src/kernel/src` filtered to
lines that are not comments is empty.

---

## Suggested order

**Done:** order zero, W-3, W-2, W-6, W-9, W-4 (with F-08), W-1, W-5 (with
F-09 and F-33), W-7's measurement and ratchet, W-11, W-12 (F-23), W-14
(F-36), W-13 (F-35), W-15 (F-37) and W-16 (F-26).
**Remaining:** F-10's tests, by module from COVERAGE-WORKLIST.md → W-8
(largest), with W-10 in parallel whenever someone can answer step 1.

W-1 landed as a split rather than a move, and took the boundary from 36
references to 29 by the gate's count of the day. What it leaves is F-09's:
six Linux-personality syscall files in the item ring that name the POSIX
process for its state. Since the gate learned to resolve module paths
(2026-09-26) the register reads 56, not 29 -- F-09 at 39, 21 of them the Linux
dispatcher's; F-07 at 12; and a new F-33, 5, a core boot check that is small
to move. W-5 took all 56 (2026-09-26): the boundary has no upward reference
left, and what the item holds of the personality is one function pointer to
its dispatcher and one `Processes` to make a native process with.

## Not on this list

F-20, F-22, F-27, F-28 and F-30 need an application, an organisation or years —
see [TODO.md](TODO.md) §6. Do not write a hazard analysis or a planning set
from here; it produces documents an assessor rejects and makes this directory
look more finished than it is.
