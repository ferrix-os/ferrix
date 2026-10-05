# Findings

The audit register for the item defined in [ITEM.md](ITEM.md). One entry per
finding, each naming what was measured, which objective it bears on, and what
would close it.

16 findings are open and 48 are closed, of 64. F-63, the queue from the chardev core to `nvrm` able to grow without bound under a spin lock, because a request abandoned before its turn stayed queued while new ones were admitted, was found by the certification consultant's review of NVIDIA's N10 text on 2026-10-05 and is pre-existing since N1e (2026-10-03); it closed the same day, when abandoned requests began to leave the queue and admission to count the queue against its room, with stage 10's chardev self-check requiring the bound. F-62, coverage evidence two landings left uncarried, so that `TRACEABILITY.md` read four verified requirements as not reached and one unverified one as reached, was found by the C1 work on 2026-10-05 and closed the same day, with the evidence carried again and a gate that fails when the kernel moved a line the evidence names and nobody carried it. F-60, a member leaving its speculation domain whose leave may miss a processor that is installing the member's space at that moment, so that the processor runs it after the leave with no barrier, was found by the certification consultant's review of `docs/OPAQUE-KERNEL.md` §9.8 on 2026-10-02 and is pre-existing since the domain landed (bf9efba95); it was open on x86-64, ARMv7-A and in the Rust memory model, and closed on 2026-10-03, ahead of §9.8's piece 2f, when the leave began to publish the domain before its scan and grace period, and every processor answering that grace period to compare its own last domain with it. F-59, a pin a live driver closed on an untranslated domain kept for good -- an unbounded leak before NVIDIA's N0f gave each device a pin budget, and since then a device that stopped pinning after enough attaches -- was found by the certification consultant's review of N0f's fix-forward on 2026-10-02 and is pre-existing; it closed the same day, when such a pin began to be given back at once and a dead driver's pins there to be quarantined and released at the next HELLO. F-58, VT-d's table writes reaching a unit whose walk does not snoop only through the processor's caches, so that a cleared entry could still be walked after its invalidation, was found by the certification consultant's review of the NVIDIA N0g design on 2026-10-02 and is pre-existing; it closed the same day, when every entry and every fresh table began to be cleaned to memory before it is published on such a unit, and an SMMUv3 without coherent walks began to be refused. F-57, a device a ring-3 driver controls able to raise any interrupt vector on x86-64 -- in any delivery mode, NMI, SMI and INIT among them -- because no interrupt remapping is programmed, was found by the certification consultant's review of `nvidia-n0` on 2026-10-02 and is pre-existing; it closed on 2026-10-03 with NVIDIA's N0g, where the machine's interrupts are isolated: every message remapped through an entry that checks its requester, compatibility format blocked on every unit, on bare metal with VT-d interrupt remapping and on the gates' patched QEMU; a GICv2m frame's residual stays recorded apart, in T.DMA path 6. F-56, the btrfs crates joining the item on 2026-10-02 without its evidence -- no structural coverage, requirements traced in part, 166 allocations still fatal and 19 name matches awaiting their arguments, no overflow lint, and two unsafe infrastructure crates in no ring -- was opened by the certification consultant the same day; its allocation gap closed the same day too, when the write path's allocations were made fallible and the reader's name matches argued. F-55, a system call copying from mapped device memory through the direct map, which stopped the machine when the window lay past the direct map, was found by os-fd on the Venus desktop on 2026-09-30 and closed on 2026-10-01 when the user-copy paths began refusing device memory with `EFAULT` through a checked alias; the render node may be opened to users as far as it is concerned. F-54, a channel read answering that its peer had closed while the peer's last message was still queued, was found by FX-1151's investigation of the net ring's check under WHPX and closed on 2026-09-29 by reading the peer's flag before the queue; the certification consultant recorded it on 2026-09-30, after the fix had landed without review. F-53, `docs/INIT.md` promising that init remounts `/` and `/data` read-only at shutdown when the kernel refused every `MS_REMOUNT`, was found by the certification consultant's review of the namespaces design and closed on 2026-09-28 with the per-mount flags of `docs/NAMESPACES.md` N1. F-48, an ARMv7-A signal handler installed without `SA_RESTORER` killed when it returned, because its return sequence was on a stack no page of which may run, was found by the W-8 reading of arch/armv7a and closed on 2026-09-27. F-50, the GICv2 distributor's read-modify-writes unlocked, so that two cores enabling neighbouring lines could leave a shared interrupt routed nowhere, was found by the W-8 reading of arch/arm_common and closed on 2026-09-27. F-47, the Security Target claiming seventeen SFRs without saying whether their dependencies were met, was found and closed on 2026-09-27 with a dependency table that justifies the three unmet ones; F-52, the conformance claim naming no CC version, was found the same day and waits on the Security Target's owner. F-21b, the TOE claiming no audit, closed on 2026-09-27 when the audit record of the TSF's own decisions was built and claimed (FAU under O.AUDIT) and authentication was the environment's (OE.AUTH). F-44, the barrier of the virtqueue the item drives left empty, so that an Arm core could let the device see an index before what it publishes, was found and closed on 2026-09-27, on an argument no emulator can test. F-45, the virtqueue indices the device shares read and written a byte at a time, was found by the stage 10 seam investigation and closed on 2026-09-27. F-43, O.WXN and ASR-2 claiming W^X for every mapping when a program may map its own pages writable and executable, was found on 2026-09-27 and waits on the customer's choice between enforcing it and narrowing the claim. F-42, a channel write refused for memory closing the handles it carried, was found by the F-10 coverage work and closed on 2026-09-27. F-13 was measured on 2026-09-27 and stays open: 37.5%, 32.6% and 28.7% of the item's object-code decisions, guards left out, took both ways on x86-64, AArch64 and ARMv7-A, and 50.2%, 48.6% and 48.8% counted by source line. F-26 closed on 2026-09-27 when every unsafe site in the item was traced to one of fourteen obligations, each tied to the requirement or hazard it serves, and the gate began refusing an untraced one. F-41, a page `mprotect` made writable after a `fork` writing into the other process's copy, was found and closed on 2026-09-27. F-10 was re-measured on 2026-09-27 over the checks written for it module by module: 89.5% of the certified item's statements on x86-64 (90.1% at its re-measure on e5f3110f, with the F-10 slices of native.rs and object/ in and 97 still needing a test, new code since measured for the first time), 90.2% on AArch64 and 84.8% on ARMv7-A, with 77, 146 and 130 still needing a test and every other unreached statement argued or put down to absent hardware. F-38, F-39 and F-40 were found by other work on 2026-09-26 and recorded by the certification review that work now goes through, and all three closed the same day with their fixes: F-39, a native process any user made running as root; F-38, a device model writing a dead driver's frames after they were given back; and F-40, a delegated job lifting its own limits, closed by a job right of its own for setting limits. F-37 closed when every kind of kernel heap a program can make and keep through the Linux personality was charged to its job against its memory limit -- thirteen kinds, each refused at the limit by a boot check while a sibling goes on -- and five leaks and missing checks the same audit found were fixed (2026-09-26); System V semaphore sets, undo records and blocked `semop` waiters, built on 2026-09-28, are a fourteenth kind, charged the same way, and System V shared memory segments, built on 2026-10-02, a fifteenth. F-35 closed when the job quotas were built -- a job's tasks, its user memory, its native objects and its share of a processor, each refused at its limit by a boot check while a sibling job goes on -- and `FRU_RSA.1` was refined to exactly those; F-37 was opened the same day for what they leave out, the kernel heap a job drives through the Linux personality (2026-09-26). F-36, a user page table freed before the shootdown that another processor's walk caches still needed, was found and closed the same day, and F-23's gate was found blind to a load file the item's own `process_create` runs, and was made to read it (2026-09-26). F-35 was opened when the vulnerability analysis was read against the code: the job quotas the Security Target claims for T.EXHAUST are not built (2026-09-26). F-23 closed when every allocation in the item was made to report failure, with a gate that counts the ones that do not (2026-09-26). F-10 is re-measured at 74.7% on x86-64, 73.7% on AArch64 and 70.9% on ARMv7-A, the 81.9% published before having been wrong, and then at 82.2% on x86-64 once two more defects of the tool were fixed and x86-64's architecture code, `trap` and `smp` were covered or argued statement by statement, F-07, F-09 and F-33 closed, which leaves the boundary with no upward reference, F-31 closed when its layout half, KASLR, was built after its side-channel half, and F-34, a writable alias of the kernel's text in the direct map, was found and closed the same day (2026-09-26). No finding here is closed by argument:
a finding closes when the thing it describes stops being true and something in
the build says so.

**Numbers reserved and not yet filed (2026-09-27).** Each is a defect already
confirmed, whose entry lands with its fix: **F-46**, xtask's power-off gates
taking a triple fault after the marker as a power-off; the fix is built on
branch `f46-power-off` (1694e217, a `FERRIX-POWER-OFF` line the gates
require) and parked there, its negative controls run and logged, to be
rebased and reviewed. **F-49**, ARMv7-A's
`psci_system` not declaring `r12` clobbered where `psci_call` does; not yet
fixed. **F-51**, the manifest's `checks.rs` test-file pattern counting the
self-check switch (`src/kernel/src/checks.rs`, product code) as verification;
closes with W-8's boot slice 21b. Until they are filed the tally above
does not count them. (F-58, reserved here on 2026-10-02, is filed below.)

**Severity.** *Blocking* — a rating cannot be claimed while it stands.
*Major* — a named objective is unmet. *Moderate* — an objective is partially
met or met without evidence. *Minor* — a defect with no objective attached yet.

| | Blocking | Major | Moderate | Minor | Informational |
|---|---:|---:|---:|---:|---:|
| Open | 2 | 6 | 7 | 0 | 1 |

Blocking: F-27 and F-28 — independent assessment and a quality management
system. Both need an organisation; neither is a defect in the code.

F-20 and F-22 were on this list until 2026-09-25, when the element was
documented as a *safety element out of context*
([SAFETY-MANUAL.md](SAFETY-MANUAL.md)) and their element-level halves were
written. The system-level halves are exported to the integrator as assumptions
of use, which is how every general-purpose certified kernel handles them.

---

## A. Boundary integrity

Measured by `tools/common/check/check-item-boundary.py`; **no upward references**, from
94 in 28 files when the audit began. The debt register in
`tools/common/data/certification-item.json` is empty since W-5 closed F-07, F-09 and F-33
(2026-09-26), and stays, empty, so that a new upward reference has to be argued
into it against a finding. `main.rs`'s 38 edges into the load are recorded
beside it, not in it: they are the composition root's ([ITEM.md](ITEM.md) §2).

**Every count this section gave before 2026-09-26 was a lower bound** -- the
29, and the 62 the audit started from. The gate matched the text
`crate::a::b` and nothing else, in source whose strings it found with a
pattern that mis-paired quotes after a `\`-newline continuation. Three kinds of
edge were invisible to it:

* **a nested `use` group.** `use crate::syscall::{exec, process}` was read as
  `crate::syscall`, the item's own dispatcher; `devmgr.rs` has exactly that;
* **a path through a name.** `syscall/mod.rs` declares `mod exec;` and calls
  `exec::sys_execveat`, and a file that writes `use crate::syscall::fd;`
  and then `fd::arg(..)` names `fd` either way. The old gate saw the second
  kind only at the `use`, and the first not at all: `syscall/mod.rs` was
  measured as naming one load module, and it names 21;
* **code read as a string.** After the first continuation in a file, the
  string pattern took code for literal text. All five of
  `arch/x86_64/paranoid.rs`'s references were in such a stretch.

The gate now resolves names the way the compiler does, short of type
checking: every `use` tree however nested, `crate`, `self` and `super`, names
bound by `use` or declared by `mod`, and `pub use` re-exports followed to the
module that defines the item. Re-measured on the same tree the register went
from 29 to 56 -- 20 more under F-09, 2 under F-07 and 5 under a new F-33 --
and none of them is new coupling. The audit's own starting tree, re-measured
the same way, has 94, not 62; the 38 paid down since are real, and F-01 to
F-08's closures stand, since each removed edges the old gate could see and the
new one confirms are gone.

### F-01 — the `Process` type is a core concept living in the Linux personality
**Closed 2026-09-26** by W-1, as a split rather than a move.

The finding was that the core named `syscall::process` -- 2,229 lines of
`fork`, `wait4` and signal bookkeeping -- because a process *is* the container
the core isolates, and the type lived in the personality. It was 10
references: 2 from the core and 8 from the item ring.

The core half is now `src/kernel/src/object/process.rs`: the address space, the
pid, the start time, the handle table, the job, and how the process ended,
with `ProcessRef`, `Control` and the pid table. The POSIX process contains it
and adds the descriptor table, the filesystem context, the signal state,
`brk`, the credentials, the family and the threads, none of which the core
type can reach. Where the core must hold a process whole it holds a `Host`, a
trait of five methods the personality implements; the scheduler holds a
thread as a one-method `UserThread`. IMPLEMENTATION.md W-1 has the design.

Of the 10 references, 4 are gone: `object/job.rs`, `object/mod.rs` and
`syscall/futex.rs` no longer name the personality, and `syscall/registry.rs`,
whose table moved into the core, went to `load` with the typed lookup it kept.
**The other 6 were not made to point into the core, and this entry does not
claim they were.** They are item-ring files naming the POSIX process for POSIX
state -- `brk`, the fd table, credentials, the dispatcher's `current()`, the
POSIX thread's signals, the Linux loader -- which is not the core-concept
defect this finding described. They are refiled where the register already
describes them: five under F-09 and `syscall/native.rs`'s under F-07.

*Verified by:* no `F-01` entry in `tools/common/data/certification-item.json`, and
nothing under `object/` or `sched/` naming `syscall::` in `check-item-
boundary.py --report`; the full boot gate row, `test-threads` and `test-jobs`.

### F-02 — the trap return path calls signal delivery directly
**Closed 2026-09-25.** Six of the seven references are gone. The frame types
`arch/*/signal.rs` needs moved to `src/kernel/src/signal_frame.rs` in the core, and
the three functions the trap return called are now reached through
`crate::trap::ReturnPath` — a struct of three function pointers the personality
registers at boot, held in an `AtomicPtr` rather than a lock because it is read
on every return to user mode.

A kernel whose personality registers nothing now returns to user mode directly,
which is the property that makes the core independently analysable.

Verified by booting all three architectures, since signal frames are
architecture-specific and the change touched every one.

### F-02a — a fault becomes a signal by an upcall from the core
**Closed 2026-09-25**, and it did not need F-01 first after all.

`ReturnPath` gained a fourth entry, `fault_signal`, and a core-owned answer
type `FaultOutcome` with three cases — delivered, ended with a pid, or no
process. `Posted` and `Origin` stay on the personality's side of the interface,
which is the whole point: a trap path that had to name them would be back where
F-02 started.

The fault *resolver* needed no interface at all, only a better question. It was
asking the Linux personality for the current process in order to reach its
address space; the scheduler already knows which address space is running, and
sets it from that same process when the task is made. `sched::current()
.address_space()` is both correct and more honest about what the fault path
means.

**`trap.rs` now names nothing above the core.** All three of its references —
`syscall::deliver`, `syscall::signal`, `syscall::process` — are gone, and the
most trusted file in the kernel is clean. Verified by booting all three
architectures with the fault path exercised: the `signal pid N ended by signal
11` lines still print, with the right pids.

### F-03 — architecture modules name the personality's `StatLayout`
**Closed 2026-09-25.** The `StatLayout` enum moved to `src/kernel/src/arch/mod.rs`,
beside the other ABI facts the facade carries; its `impl` stayed in
`syscall/stat.rs`, which is legal within a crate. Data in the core, behaviour
in the personality, and the dependency now points downward. 62 upward
references became 59.

### F-04 — the core device registry names STM32MP1 board support
**Closed 2026-09-26.** All six of the manifest's F-04 entries are gone: the
three from `device.rs` into `stm32mp1`, `stm32mp1_gpu` and `stm32mp1_usb`, and
the three from the item that reached the same board for its boot mode
(`power.rs`, `syscall/system.rs`) and its pixel clock (`syscall/native.rs`).

The registry now keeps a list of `BoardBinding`s: a binding number, a
`prepare` function that answers with registers, an interrupt, a DMA shape and
a line for the log, and the one clock a driver may set, if there is one. The
list is a `Hooks` (`src/kernel/src/hooks.rs`, core), a handful of `Once` cells
read without a lock, because board support waits on the timer while it
prepares a device. `stm32mp1::install` registers the display, the USB host and
the GPU, in the order their nodes were published before, and hands power the
function that writes U-Boot's boot mode. The registry mints the apertures and
the vector from what `prepare` says under the rules it applies to every node,
so board support still cannot hand a driver memory the kernel owns.
`device_clock` asks `device::board_clock` for the node's binding's clock.

Registration is one explicit call in `main.rs`'s `register_load`, before
enumeration, not a link-time table. The boot prints what was registered and
stops with FX-0006 if anything is missing.

*Not verified on the board.* QEMU's machines carry no STM32MP15 device tree,
so under every gate the three bindings find nothing. The DK1's display, USB
host and GPU were not enumerated again on hardware for this change. The path
is the old one reshaped -- same order, same minting, same log lines -- and the
next board session should confirm that the `display`, `usb` and `gpu` lines
are unchanged.

### F-05 — `claim.rs` names `block_ring`
**Closed 2026-09-25.** Only the `StillServed` enum was wanted, and it belongs
to the claim rather than to the ring: a quiesce asks whether anything still
serves a node, and the answer must not depend on which uncertified subsystem
happens to be serving it. Moved to `src/kernel/src/claim.rs`; `block_ring`,
`render`, `display` and `native` now answer with the core's type.

### F-06 — core names two item-ring modules
**Closed 2026-09-26** by W-1. The three references were `object/job.rs` to
`syscall::registry`, to find a job's members, and `sched/mod.rs` and `sched/
task.rs` to `syscall::thread`, because a task held the POSIX thread. The pid
table is the core's now, and a task holds a `sched::UserThread` -- the
scheduler's view of a thread, which is the process it runs in and nothing
POSIX. Inner-ring only, so no present rating moved; it is the ratchet's path
to an EAL6+/ASIL D `core`, and `object/` and `sched/` now name nothing above it.

### F-07 — the native ABI dispatcher fans out across the load ring
**Closed 2026-09-26** by W-5. All 12 references are gone: 10 from
`syscall/native.rs` into `block_ring`, `net_ring`, `fs::cgroupfs`, `display`,
`render`, `input` and the personality's `exec`, `load`, `fd` and `process`,
and 2 from `devmgr.rs` into `syscall::exec` and `syscall::process`.

The item defines three interfaces and the load registers into them from
`main.rs`'s `register_load`, as F-04 and F-08 did:

* **A table of handlers** for the six calls that are about a subsystem above
  the item -- `block_ring_create`, `net_ring_create`,
  `display_control_create`, `render_control_create`,
  `input_control_create` and `job_for_cgroup` -- keyed by call and
  registered by the subsystem (`native::serve`). The device handle, its
  `MANAGE` right and the driver's handle stay in the item
  (`native::control_channel`); only the channel is the subsystem's to make.
* **`native::Processes`**, which the Linux personality lends from
  `syscall/launch.rs` beside init's `Launcher`: load an image into a new
  process, and claim, prepare and start it with an argument taken between
  the prepare and the run. `process_create`, `process_start` and `devmgr`'s
  own start all use it; the job, the rights and the handle move stay in the
  item.
* **`native::Server`s**, which a quiesce waits out and releases in the order
  registered: the block ring, the net ring, the display, the renderer, input
  and audio, at most eight.

The dispatcher finds its caller through the scheduler's `UserThread` and hands
handlers the core's `Process`, and the table's the caller as a `Host`.

*The guarantee the `match` gave.* It was exhaustive, so an unanswered call did
not compile. It still is, for every call the item answers. The ones it leaves
to the table are checked at boot instead, on every boot: `main.rs` stops with
FX-0006 if any has no handler, and says what it found (*"10 native calls
answered above the item, 6 subsystems a quiesce waits out"*). The table is
searched by the decoded call, never indexed by a program's number, so F-31's
clamp in `decode` is still the only bound a misprediction could cross.

*Verified by:* no `F-07` entry in the manifest, and nothing from `native.rs`
or `devmgr.rs` above the item in `--report`; the full boot gate row, in which
devmgr starts its drivers through `Processes` and the block ring's check
drives the table, and `test-net`, which makes a net ring through it.

### F-08 — bring-up and power name the filesystem
**Closed 2026-09-26.** All six entries are gone, from `init.rs`, `power.rs`
and `devmgr.rs` into `fs`, `fs::root_disk`, `fs::data_disk`, `block_ring` and
`syscall::load`. Each consumer in the item now defines what it needs, and the
load ring registers into it from `main.rs` before the first use:

* **Power** keeps a list of `Flush`es -- a mount point and a commit -- and
  commits every one, in the order registered, before the machine stops.
  `fs::install` registers `/` and then `/data`, so a power-off still commits
  both before power goes, in the old order. `test-shell`'s `/data/k7`
  surviving `poweroff -f -n`, and `test-powerfail`, both pass.
* **Init** decides which program runs and says how it ended; how a program is
  opened and started is a `Launcher` that `syscall/launch.rs`, in the load
  ring, registers. That also removed `init.rs`'s use of `syscall::exec`,
  which the gate never reported (below).
* **`devmgr`** reads its program and drivers through a `ReadFile` the
  filesystem registers. `location_of` -- the PCI location devmgr's messages
  and every ring's HELLO name a device by -- moved from `block_ring` into
  `devmgr.rs`, and `block_ring` re-exports it.

`main.rs` checks, on every boot and before anything uses them, that a flush,
the launcher and the reader are registered (FX-0006).

*What the gate did not see.* When this closed, two kinds of edge from the item
into the load ring were invisible to `check-item-boundary.py`, and closing
this finding did not claim them. Both are measured since 2026-09-26. The
`use crate::syscall::{exec, process}` in `devmgr.rs` is F-07's, where its two
edges are now filed. And `main.rs`, the crate root, calls the load ring by
bare module paths (`fs::install`, `syscall::launch::install`,
`stm32mp1::install`, `fs::init`, `fs::root_disk::init`): those are the
composition root's 32 edges, listed in the manifest apart from the debt
register ([ITEM.md](ITEM.md) §2). None of it was F-08's: `init.rs` and
`power.rs` name nothing in the load by any route.

### F-09 — item-ring syscalls reach personality modules
**Closed 2026-09-26** by W-5. All 39 references are gone: 3 from the `arch`
trap entries into `syscall`, 21 from the Linux dispatcher `syscall/mod.rs`,
and 15 from `syscall/{futex,limits,memory,system,thread}.rs` into the
personality's state. Each part took a different answer, because each was a
different question.

* **The trap entries** now call `crate::trap::system_call`, which answers
  through a `SyscallEntry` the core holds in a `Once` and `main.rs` registers
  (`syscall::dispatch`), beside the `ReturnPath` the personality registers
  for the way back. `SyscallArgs` and `Outcome`, the trap path's own contract,
  moved into `trap.rs`. With nothing registered a call is `ENOSYS`.
* **The Linux dispatcher** is split where the item's job ends. `syscall/mod.rs`
  keeps the way in, the native range, and the Linux number decoded by
  `arch::decode_syscall` with F-31's clamp in front of the table; it hands the
  decoded call to a `Personality`, a trait the item defines, which
  `syscall/linux.rs` -- the routing through the personality's modules, in the
  load ring -- implements. `main.rs` composes the two at compile time,
  registering `dispatch_with::<Linux>` as the core's entry.
* **The five files** were each asked this entry's question -- does the state it
  wants belong in the core, or is the file the personality's -- and each is
  the personality's: `futex(2)`, the rlimits and `sched_*` calls, `mmap`'s
  argument decoding onto the core's `AddressSpace`, `uname`/`sethostname`/
  `reboot(2)`'s checks over POSIX credentials, and the POSIX thread. Nothing
  in the core or the item calls them once the Linux dispatcher is above the
  item, so they moved to `load` in the manifest with no code change and no new
  edge. [ITEM.md](ITEM.md) §2 argues the move file by file.

*What the item gave up.* The item ring's product code went from 10,578 lines
to 8,002; the five files were 2,330 of it, and the Linux dispatcher's routing
most of the rest. None is
code the Security Target's claims rest on: it names the personality a threat
agent outside the TSF, and its quota is the job's, in the core. The credential
checks in `limits.rs` and `system.rs` are the personality's policy over
identities the ST claims nothing about. What that does mean, and did before, is
that load code runs in ring 0 and shares the kernel heap: `futex.rs`'s waiter
allocations are program-driveable, and since F-23 closed for the item they are
among the load's allocations that still stop the machine when the heap refuses
them (FX-0008, AoU-5).

*Cost.* Each system call now pays one indirect call, through the core's
`Once`, where it paid none. The first shape held the personality in a second
pointer, and that showed: two million `read`/`write` calls under KVM (busybox
`dd bs=1`, 54 runs each, alternating boots) took a median 1.244 s against
1.185 s, +5.0%. Composed at compile time instead, 1.257 s against 1.242 s,
+1.2% (minimum +1.8%), which is inside this host's noise; the commit
"Compose the Linux personality with the dispatcher at compile time" has the
table. No lock and no allocation were added to the path.

*Verified by:* no `F-09` entry in the manifest, and nothing from `arch/` or
the item above its ring in `--report`; the full boot gate row, `test-threads`,
`test-jobs`, `test-net`, and `test-boot --mitigations off`.

### F-33 — a core self-check loads a Linux program
**Closed 2026-09-26** by W-5. All 5 references are gone: `arch/x86_64/
paranoid.rs` no longer names `syscall::exec`, `syscall::image`, `syscall::load`,
`syscall::process` or `fs`.

The x86-64 NMI and `#DB` entry's boot check -- which builds an ELF, loads it
with the Linux loader and starts it, to prove a breakpoint in the `SYSCALL`
trampoline fires and returns with the kernel's `GS` -- moved whole into
`arch/x86_64/paranoid/check.rs`. That is the first of the two ways this entry
said it could close: the file matches the manifest's `check.rs` test pattern,
so it is counted as the verification it is, and reaching the load ring for a
fixture is what a check may do. It is a child of the entry's module, so it
reads the entry's counters without the entry exporting them. The entry keeps
only `debug_hook`, which its own handler runs. The boot's lines are unchanged.

---

## B. Verification

### F-10 — statement coverage is 84.5–90.1%, not 100%
**Major**, re-measured 2026-09-26 on main at a6d505a2, with KASLR. Every boot
gate that exercises the item now contributes, on every architecture: the
certified item is 5,103 of 6,828 statements on x86-64 (74.7%), 5,189 of 7,041
on AArch64 (73.7%) and 4,895 of 6,908 on ARMv7-A (70.9%). `cargo xtask coverage` runs the
suite and fails below the floor `coverage-floor.json` records, which is W-7's
ratchet.

**The 81.9% published on 2026-09-25 was wrong**, and not in one direction.
`coverage-report.py` had two defects (VERIFICATION.md §3.4): a search sentinel
below the higher half that skipped every block starting exactly on a
statement, which under-reported x86-64 and AArch64 by about a third, and a
union that read every gate's trace against one kernel although the gates build
different ones, which over-reported by crediting statements nobody ran. The
figure was the two netted against each other. The corrected suite, with eight
more gates in it than the published one had, reads seven points lower.

The four gates that used to write an empty trace -- `test-btrfs`,
`test-shell`, `test-sysfs`, `test-restart` -- ended by killing QEMU, and the
plugin writes its table only when QEMU exits. xtask now asks QEMU to stop
before killing it, and numbers each boot's trace so a gate that boots several
times keeps them all. AArch64 also boots once on a GICv3, the Pixel 7's
interrupt controller, which the default `virt` does not have.

The residual is enumerated per architecture (`coverage-residual-<arch>.json`)
and sorted in [COVERAGE-RESIDUAL.md](COVERAGE-RESIDUAL.md). On x86-64, **146
statements are argued** -- 131 of another architecture's code, 15 reached only
when stopping -- 259 are a statement about which machine was measured, and
**1,320 simply need a test**; AArch64 owes 1,481 and ARMv7-A 1,446.
[COVERAGE-WORKLIST.md](COVERAGE-WORKLIST.md) groups them by module for the
test-writing that closes this.

**x86-64, later the same day** (on main at 195a2e93): 5,533 of 6,735
statements, **82.2%**. Most of the rise is the tool, which had two more
defects (VERIFICATION.md §3.4): lines whose only rows were in function copies
the linker discarded counted as unreached statements, about a quarter of the
residual, and rows after a line-table sequence end were filed under the wrong
file, crediting `uaccess.rs` with other files' statements. The rest is the
first module-by-module pass: `arch/x86_64`, x86-64's share of `arch`, `trap`
and `smp` have **nothing left that needs a test**. Stage 3 on x86-64 runs
programs that end by their own divide error, invalid opcode, unmapped read,
privileged instruction, x87 exception and read past a mapped file's end, and
asks `arch_prctl` both refusals; stage 4 checks shootdown page sets; the suite
boots a PC without an HPET or RDSEED, a reset and a single processor. What
those modules still do not reach, 74 statements, is argued line by line in
`coverage-argued-x86_64.json`: the stopping path, 9 defensive paths, one
statement a test runs but the line table credits elsewhere, and hardware the
TCG machine lacks but gate 6's KVM boot has (the invariant TSC, the speculation
controls). x86-64 still owes **757** statements a test, in the other modules;
the Arm figures above predate the tool's fixes.

**All three, 2026-09-27** (on main at c14846aa, measured on 9076655c and
carried across the kernel relayout, which moved files without changing a
statement of the item): x86-64 **6,902 of 7,664, 90.1%** (re-measured 2026-09-27 on e5f3110f); AArch64 **6,824
of 7,587, 89.9%** and ARMv7-A **6,251 of 7,400, 84.5%** (both re-measured 2026-09-27 at 9e196852). Three passes wrote
the checks, one per part of the item, and the suite measured them together:

* *The memory layer and the objects* (`user/`, `object/`, `mm`, `vmap`,
  `early`): each operation of the memory layer run once per allocation it
  makes with that allocation failed (`fallible::inject_once`), the refusals
  of an address space and of the core objects, the paths no program takes,
  and every core error and object formatted as a diagnostic would. It found
  and fixed three leaks and wrong answers under failure: the page tables a
  fault that ran out of memory left, a forked space's root when the space
  could not be made, and a private file write that could not record its copy.
* *The Arm architectures* (`arch/aarch64`, `arch/armv7a`):
  `arch::check_machine` (FX-0308) decodes a trap frame of every exception
  class and fault status into Linux's signal and masks lines at the
  interrupt controller; programs end by their own exceptions; the
  speculation tables are held to Linux's for cores QEMU does not have; and
  the suite boots AArch64 from its device tree (with a GICv2 and as the
  Pixel 7's GICv3), with `nosmp` and into a reset, and ARMv7-A on a
  Cortex-A15, one processor, 3 GiB and into a reset.
* *The kernel's services* (`sched`, `syscall`, `iommu`, `devmgr`, `claim`,
  `hooks`, `console`): the waits before the scheduler, the reaper under
  failure, the native calls' refusals (FX-0903), the small services the
  item leans on (FX-0904), and a boot with the command-line options no
  other gate gives.

What those reach no further is argued statement by statement in
`coverage-argued-<arch>.json` -- 275, 264 and 329 arguments over 368, 320
and 485 statements -- among them the quarantine F-38 added, whose check
arms are failure paths and whose whole machinery is another architecture's
on ARMv7-A, which programs no IOMMU unit. **77 statements on x86-64, 146 on
AArch64 and 130 on ARMv7-A still need a test**; COVERAGE-WORKLIST.md lists
them by module, `syscall/native.rs`, `user/space.rs`, AArch64's `trap` and
`arch/aarch64` and `object/` the largest. The floors are 88.5, 89.0 and
83.5.

*Closes when:* the *needs-a-test* category is covered or individually
justified on every architecture. The argued categories have their argument
written.

### F-11 — coverage measures the debug profile, the item ships release
**Closed 2026-09-25.** The release profile is now measured:
`coverage-x86_64-release.json`, 75.2% of the item from one boot against the
debug profile's 71.6% on the same gate (re-measured 2026-09-26 with the
corrected tool; the figures first published were 47.6% and 46.6%, F-10 says
why).

The finding's premise was right and its expected consequence was wrong. The
percentage moves a few points; the *denominator* moves by a third, 6,828
statements to 4,462, because optimisation leaves fewer distinct `is_stmt` rows
to reach. So the number survives a change of profile and the population being
counted does not, which is the thing a submission has to state. VERIFICATION.md
§3.2.

### F-12 — coverage is x86-64 only
**Closed 2026-09-25.** Every architecture in the reference configuration can be
measured, which is what this finding asked; since 2026-09-26 each has the
whole suite and not one boot: `coverage-aarch64.json` at 73.7% and
`coverage-armv7a.json` at 70.9% of the item.

The gap first recorded between them -- ARMv7-A at 70.8% from one boot against
46.1% for AArch64, put down to x86-64 and AArch64 carrying more arch-specific
code -- was the tool's sentinel defect, which only 64-bit addresses met. The
three agree within four points.

### F-13 — no decision or MC/DC coverage
**Informational.** Not required at DAL C. Required at DAL B and DAL A, and the
present method (basic-block granularity) cannot produce MC/DC without
instrumenting conditions.

**Measured 2026-09-27** (main at 88d9ce39, debug profile, the §3.1 suite):
decision coverage of the item's *object code*, from the drcov traces the
statement figure already uses, by `tools/common/gen/decision-coverage.py`. A
conditional branch has taken both ways when executed blocks began at its
target and at its fall-through. Of the certified item's conditional branches,
leaving out the guards -- the overflow, bounds, `unwrap`, assertion and
precondition checks and `fatal!`s whose other way is a panic, 1,906, 1,814
and 1,775 of them -- **37.5% on x86-64 (3,965 of 10,571), 32.6% on AArch64
(2,994 of 9,192) and 28.7% on ARMv7-A (3,267 of 11,378)** took both ways.
Those are upper bounds; the lower bound, counting a successor only when no
other edge into it ran, is 10.4%, 8.9% and 9.8% of all branches. Counted by
source line instead -- a decision covered when any compiled copy of it took
both ways -- **50.2%, 48.6% and 48.8%**. VERIFICATION.md §3.6 gives the
method and its limits; `decision-coverage-<arch>.json` the rings and files.

Most of the object-code gap is a few source decisions multiplied by inlining
and monomorphisation: `timer::now_nanos`'s `hz == 0`, in more than 400
functions; the deadline tests of the generic waits in `sched/wait.rs`; the
`?`s of `object/quota.rs`'s slot lookup; x86-64's interrupt restore in every
lock guard; the Arm pair's `copy_to_user` results.

*Would be tightened by:* QEMU's `cflow` plugin, which records edges rather
than blocks and would close the gap between the two bounds; a measurement of
the release profile, which drops most guards; an analysis of the
object-to-source correspondence DO-178C §6.4.4.2b asks before object-code
coverage stands in for source coverage. *Stays open* at DAL C, as
Informational; at DAL B it would be Major, and MC/DC at DAL A is out of this
method's reach.

### F-14 — tests are not traced to requirements
**Advanced 2026-09-27 (W-8 steps 1 and 2).** The trace exists as a gate:
`tools/common/check/check-traceability.py`, in `cargo xtask check`, reads the
`/// Verifies:` tags on check functions, fails on an id no requirement
defines, and holds the unverified requirements in a baseline that may only
shrink; TRACEABILITY.md is generated from it with, per architecture, whether
the verifying check ran (the coverage run records the checks' own statements
from its next run on; until then *not measured*). Of 51 high-level
requirements **1** is named by a check (`H.MEM.7`, by
`smp/check.rs`'s two shootdown checks) and 50 are in the baseline. *Still
open:* the tags, subsystem by subsystem, and the run-time column's evidence.

**Advanced 2026-09-27 (W-8 step 3, the `object/` pilot).** 57 check
functions and host tests name a requirement. Of `object/`'s 103 low-level
requirements **78 are verified** and 25 are in the baseline as checks to
write; of the 63 high-level ones **18** are verified (`H.MEM.7`, `H.MEM.11`,
`H.OBJ.3`, `4`, `7`, `9`, `11`, `13` to `17`, `H.QUOTA.1` to `4`, `6`, `7`),
45 in the baseline. A check names a requirement only if it proves the whole
criterion alone; where checks prove parts, the requirement was split.
*Still open:* the other subsystems' tags, the 70 baselined requirements,
and the run-time column's evidence (the next `cargo xtask coverage`).

**Advanced 2026-09-27 (W-8 step 4, `iommu`).** 65 check functions, host
tests and xtask gates name a requirement. Of `iommu`'s 43 low-level
requirements **16 are verified** and 27 are in the baseline as checks to
write; the quarantine's three `L.object` ones are verified now that their
check is in a check file. Of the 66 high-level ones **22** are verified
(`H.DMA.2`, `3`, `4` and `6` added), 44 in the baseline; 93 baselined in
all. *Still open:* the other subsystems' tags, the 93, and the run-time
column's evidence.

**Advanced 2026-09-27 (W-8 step 4, `mm` and `user`).** 151 check functions,
host tests and xtask gates name a requirement. Of memory's 166 low-level requirements
**111 are verified** (27 of 61 `L.mm`, 84 of 105 `L.user`) and 55 are in the
baseline; of the 74 high-level ones **31** are verified. Stage 2's memory
check and the W^X and sealed sweeps moved from `main.rs` and `mm.rs` into
`mm/check.rs` so a tag could go on them; `H.MEM.7`'s two tags, each on a
check proving one half, became `H.MEM.7` and `H.MEM.12`, one each. *Still
open:* the other subsystems, 147 baselined requirements, the run-time column.

**Advanced 2026-09-27 (W-8 step 4, `smp`).** 158 check functions, host
tests and xtask gates name a requirement. Of smp's 32 low-level
requirements **9 are verified** and 23 are in the baseline, among them the
bound on a wait for a processor that never answers, proved only by a
negative control recorded in a commit message; of the 77 high-level ones
**33** are verified (`H.SCHED.6` and `H.MEM.19` added). *Still open:* the
other subsystems, 171 baselined requirements, the run-time column.

**Advanced 2026-09-27 (W-8 step 4, `console`).** 204 check functions, host
tests and xtask gates name a requirement. Of console's 41 low-level
requirements **12 are verified** -- the log ring's wrap, overrun, partial
read and racing writers, what the console records and keeps out of the log,
the recent-output ring, a failure report's line on the port, and the Arm
ports' console choices -- and 29 are in the baseline, among them a report
written past a lock nobody releases and every case of the transmit ring;
two checks that would verify three more, `console::input::check` and
`console::output::check`, live in product files. Of the 95 high-level ones
**44** are verified (`H.TRAP.13`, a log read, added). *Still open:* the
other subsystems, 275 baselined requirements, the run-time column.

**Major.** The boot gates assert rich properties — 2,387 mappings swept for
W^X, 16 of 16 interrupt deliveries waking their waiter — but nothing links an
assertion to a requirement id. `docs/sysml/` has 33 requirements and 32
`verify`/`objective` links, at system granularity.

Requirements-based testing is the spine of DO-178C, 62304 §5.6-5.7 and
EN 50716; without the trace, the tests are evidence of *something* rather than
evidence *for* something.

### F-57 — on x86-64 a device a driver controls can raise any interrupt vector
**Moderate**, opened 2026-10-02 (the certification consultant, at the review
of `nvidia-n0` 0a619332e). Pre-existing: not introduced by that branch.
**Closed 2026-10-03** by NVIDIA's N0g (`docs/NVIDIA.md` §12.3), where the
machine's interrupts are isolated.

An MSI or MSI-X interrupt on x86-64 is a DMA write to the local APICs'
range, 0xFEEx_xxxx, carrying the vector in its data
(`src/kernel/src/arch/x86_64/msi.rs`). VT-d does not translate a write to
that range as DMA; only its interrupt remapping, which the kernel did not
program (`intremap=off` on xtask's machine, and nothing written to the
unit's interrupt remapping table on any), would check it. So a device whose
ring-3 driver programs it -- or whose firmware does, as a GPU's GSP firmware
does -- could raise any vector on any processor: another device's, the
timer's, or the kernel's own IPIs, the TLB shootdown's among them. A
compatibility-format message also chooses its **delivery mode**, not only
its vector: NMI, SMI and INIT as well as fixed, so a device could send an
INIT and stop a processor, or an SMI into firmware. The kernel's handlers
tolerate a spurious delivery of most vectors, but not all of them are
idempotent, and a storm on a vector the kernel owns is bounded by nothing a
driver acknowledges. `VULNERABILITY-ANALYSIS.md`'s T.DMA did not list the
path.

It matters most once a driver holds a device that runs firmware of its own:
NVIDIA's N1 hands the RTX 3060 and its GSP firmware to `nvrm`. On AArch64 a
GICv3 ITS translates messages by device ID; under GICv2m, every device whose
domain maps the frame's doorbell can raise any of the frame's message SPIs,
its siblings' included, though no SGI, PPI or kernel IPI. That is recorded
apart, as T.DMA path 6's GICv2m residual, and is not part of this finding's
closure.

*Closed by* N0g:

* **Interrupt remapping on every VT-d unit that offers it**, with queued
  invalidation (N0g's first slice): an interrupt remapping table per unit
  in xAPIC format, and `IRE` set without `CFI`, with `CFIS` read back clear
  (`L.iommu.50`); a unit that does not block compatibility format counts as
  not isolating.
* **Remappable messages**: every vector minted is an entry, written while
  not present and invalidated before its message is programmed
  (`L.iommu.52`), whose delivery mode is always fixed and which checks the
  requester's whole source ID (`L.iommu.53`, `L.x86_64.129`); a function
  the units cannot tell apart from another gets no vector (`L.device.28`).
* **The console's I/O APIC line** converted to a remappable entry of its own
  at bring-up, masked before it is rewritten (`L.x86_64.130`); no vector in
  compatibility format exists when remapping goes on (check R9).
* **Isolation stated per device** (`device_isolation`, `L.iommu.55`), and a
  device marked for isolated interrupts refused vectors and pins where the
  machine's are not (`L.device.27`): `devmgr` marks every device that runs
  firmware of its own, and starts no driver for one it could not mark.
* **x2APIC** left on by firmware taken back to xAPIC on every processor, or
  the boot stopped by name (`L.x86_64.131`).
* **The reference configuration on QEMU** is the patched QEMU 10.2.1
  (`ferrix-cfi`), whose unit implements `CFI`/`CFIS` and blocks
  compatibility format with fault 0x25: stock QEMU passes it through
  whatever `CFI` says, so without the patch no QEMU configuration isolates
  interrupts. xtask boots x86-64 only on it, with `intremap=on,eim=off` and
  the split interrupt controller under KVM.

Shown on x86-64 under KVM and TCG by checks R1 to R10 (`docs/NVIDIA.md`
§12.3): a compatibility-format message to a vector of the check's own and
one in NMI mode refused with fault 0x25 and delivered nowhere, a message
naming another function's entry refused with 0x26, the console's line
delivered only through its own entry, every invalidation queued, a failed
one releasing nothing, x2APIC handled per processor, the mark's refusal.
Each has a negative control that fired. R2's, and R1's second half -- a
forged 0xFC after the check's window -- cannot be one-file controls, since
with `CFI` kept R1 fires first: they were run by hand as two-file controls,
recorded with commit, tree, cleanliness, diff and the line that fired, as
the consultant's ruling of 2026-10-02 (ledger line 262) asks. INIT is never
forged, since a delivered INIT would stop a processor.

*The `ferrix-3060` domain* runs the same patched QEMU as its emulator, so
the closure covers it, and N1's hand-over is fail-closed: `devmgr` marks the
GPU, and the kernel refuses its vectors and pins unless `device_isolation`'s
bit 1 holds in that guest. The domain's isolation is verified at N1's first
boot by that bit. **Verified 2026-10-03**: the first boot of the RTX 3060 in
`ferrix-3060` (`cargo xtask run-nvidia`, the patched QEMU at
`/usr/local/lib/ferrix/qemu`) printed `nvrm: device_isolation 0x3:
interrupts isolated (bit 1), DMA translated` for the card at guest
`02:00.0`, behind a root port, in a translated VT-d domain.

*Where it stays open:* a machine without VT-d interrupt remapping, or with
a unit that does not block compatibility format, or a PCI function behind
no unit -- there `device_isolation`'s bit 1 is clear and a marked device is
refused; AArch64 under GICv2m (above) and ARMv7-A, as their own residuals.
On AArch64 behind a GICv3 ITS the bit is not yet set either, since that path
is Arm code N0g leaves alone: a device that runs firmware of its own cannot
be driven on AArch64 until the ITS path sets it. That fails closed.

### F-56 — the btrfs crates joined the item without the item's evidence
**Major**, opened 2026-10-02 (the certification consultant, at the review of
the boundary landing). The customer put `ferrix-btrfs` and
`ferrix-btrfs-write` in the item that day: 14,291 lines of product code
([ITEM.md](ITEM.md) §2), about a sixth of it. The boundary, unsafe, panic,
complexity and fallible-allocation gates read them from that landing on. What
the kernel's part of the item has and they do not:

* **No structural coverage.** Their evidence is 8,278 lines of host tests.
  `coverage-report.py` and `decision-coverage.py` measure the kernel's boots
  only, so F-10's and F-13's figures, and `coverage-floor.json`, say nothing
  about the crates, and nothing has measured what their tests reach.
* **Requirements traced in part.** `H.STORE.1` to `H.STORE.8` and
  `L.btrfs.1` to `L.btrfs.116` are written (parts 13 and 24), each narrow
  enough that one check proves it whole; 113 of the 116 low-level ones and
  all 8 high-level ones are named by a host test or gate that does, and
  `L.btrfs.9`, `L.btrfs.12` and `L.btrfs.23` wait in the baseline
  (TRACEABILITY.md). The write path has no
  fuzzer (V-09).
* **Closed 2026-10-02: allocations that stopped the machine.** The write
  path's 166 were made fallible (branch `btrfs-fallible`): vectors through
  `ferrix-fallible`'s `try_reserve` helpers, map inserts in the kernel's
  reserved section through a dispatch the kernel installs at boot
  (`L.mm.63`), and a refusal answered `OutOfMemory` and `ENOMEM`. The
  reader's 19 name matches are argued `NOALLOC:`. The gate counts 0 in
  either crate and `cargo xtask check` holds them there (`L.btrfs.22`);
  host tests fail every allocation of an open, a read, a log replay and a
  script of operations and a commit in turn (`H.STORE.7`, `L.btrfs.108` to
  `L.btrfs.116`). F-23's "no allocation in the item's source is fatal" now
  holds for the crates too; heap exhaustion still aborts a shared mount's
  transaction for every partition (VULNERABILITY-ANALYSIS.md, T.EXHAUST
  path 6).
* **No overflow lint.** The release kernel builds without overflow checks, so
  an unchecked sum of two values read from a hostile volume wraps silently
  (and panics under `dev` and `iterate`, which keep them); clippy's
  `arithmetic_side_effects` reports 112 sites in the two crates and is not
  denied (`L.btrfs.23`).
* **Infrastructure in no ring.** The crates may depend on `ferrix-fallible`
  and `ferrix-sync` through the manifest's `infrastructure_allowlist`, which
  the core already links. Neither is classified, and the item's unsafe,
  panic and complexity gates do not read them, though both are `unsafe` code
  the core's soundness rests on (9 and 50 lines naming `unsafe`, by grep).

*Closes when* the crates' coverage under their tests is measured, committed
and ratcheted with the residual argued (TODO.md §4.7 item 3); every
`L.btrfs.*` and `H.STORE.*` is named by a check and the baseline holds none
of them; the fallible-allocation gate records no site in either crate (met
2026-10-02); the overflow lint is denied for both with no site left; and `ferrix-fallible`
and `ferrix-sync` are classified `core` and read by the gates, or argued out.

---

## C. Requirements

### F-15 — no low-level requirements
**Advanced 2026-09-27 (W-8 steps 1 and 2).** The level above them and their
format exist. `docs/sysml/13-item-requirements.sysml` holds **51 high-level
requirements** (`H.MEM`, `H.OBJ`, `H.SCHED`, `H.IRQ`, `H.DMA`, `H.TRAP`,
`H.BOOT`, `H.QUOTA`, `H.FAIL`), decomposed from the eight objectives of
SECURITY-TARGET §8.2 and ASR-1 to ASR-8 so that each of the sixteen has at
least one; and `ItemLowLevel`, whose `unit` the gate resolves to a function
of the item's product code. **0 low-level requirements** are written: the gate
reports all 2,335 product functions as named by none (without failing).
*Still open:* every low-level requirement; W-8 step 3 (`object/`) is next.

**Advanced 2026-09-27 (W-8 step 3).** `object/` is the first subsystem with
its low-level requirements complete: **103** `L.object.*` in
`docs/sysml/14-object-requirements.sysml`, each naming the functions that
carry it. Of its 261 product functions 173 are a requirement's unit, 83 are
accessors (one statement or expression, no branch, no `unsafe`: covered by
the requirement they serve, the gate's rule) and 5 check code awaiting a
move; **0 are named by none**, and the gate fails a new function there that
no requirement names. Item-wide, 1,469 of 2,335 product functions are still
named by none. *Still open:* the other subsystems (W-8 step 4).

**Advanced 2026-09-27 (W-8 step 4, `iommu`).** `iommu` is the second
subsystem complete: **43** `L.iommu.*` in
`docs/sysml/16-iommu-requirements.sysml`. Of its 95 product functions 76 are
a requirement's unit and 19 accessors; **0 are named by none**. Four boot
checks moved out of product files into check files (`check_iommu`,
`check_dma_faults`, `check_domains`, `check_quarantine`), so the item has
2,332 product functions (2,338 before), of which 1,393 are still named by
none. *Still open:* the other subsystems.

**Advanced 2026-09-27 (W-8 step 4, `mm` and `user`).** 166 more:
`L.mm.1` to `L.mm.61` and `L.user.1` to `L.user.105` in
`docs/sysml/17-memory-requirements.sysml`. Of the 297 product functions of
`mm`, `vmap`, `early` and `user`, 236 are a requirement's unit, 49
accessors and 12 check code; **0 are named by none**, and the gate holds the
four as complete. Item-wide, 1,178 of 2,330 product functions are still
named by none. *Still open:* the remaining subsystems.

**Advanced 2026-09-27 (W-8 step 4, `smp`).** 32 more: `L.smp.1` to
`L.smp.32` in `docs/sysml/22-smp-requirements.sysml`. Of smp's 57 product
functions 38 are a requirement's unit, 17 accessors and 2 check code
(`run_everywhere`, `next_job`); **0 are named by none**, and the gate holds
`smp` as complete. Item-wide, 1,145 of 2,345 product functions are still
named by none. *Still open:* the remaining subsystems.

**Advanced 2026-09-27 (W-8 step 4, `console`).** 41 more: `L.console.1` to
`L.console.41` in `docs/sysml/23-console-requirements.sysml`, two of them
on the Arm ports' console choice. Of console's 74 product functions 62 are
a requirement's unit, 10 accessors and 2 check code
(`console::input::check`, `console::output::check`); **0 are named by
none**, and the gate holds `console` as complete. Item-wide, 874 of 2,346
product functions are still named by none. *Still open:* the remaining
subsystems.

**Major.** 33 requirements exist, all at system level (`<'G.1'>` kernel
threads, `<'G.2'>` address-space scale). DO-178C needs high- and low-level
requirements with the design between them; 62304 §5.4 needs detailed design
down to the software *unit*; EN 50716 needs a Software Requirements
Specification traced to components.

49,431 lines of item product code trace to 33 requirements.

### F-16 — requirements are narrative, not verifiable
**Advanced 2026-09-27 (W-8 steps 1 and 2).** The item's requirements are no
longer prose: each of the 51 high-level ones carries a `statement` with
*shall* about observable behaviour and a pass/fail `criterion`, counted where
there is a count, and the gate refuses one without either. The 33 goal-level
requirements of `01-requirements.sysml` stay rationale, as they should: they
are the parents, not the requirements a test discharges. *Still open:* the
same for every low-level requirement as it is written.

**Advanced 2026-09-27 (W-8 step 3).** The 103 `L.object.*` requirements
carry both, and the pilot tightened the rule: a statement says no more than
its criterion tests. Applying it split or restated eight of step 2's
high-level ones in object/'s areas (63 high-level now) and found four in
other areas to propose (`H.SCHED.3`, `H.OBJ.2`, `H.IRQ.1`, `H.QUOTA.5`;
IMPLEMENTATION.md W-8). *Still open:* the other subsystems' low level.

**Advanced 2026-09-27 (W-8 step 4, `iommu`).** The 43 `L.iommu.*`
requirements carry both, and the same rule split two of step 2's: `H.DMA.2`
into `H.DMA.2`, `6` and `7`, and `H.DMA.3` into `H.DMA.3` and `8`, one
check able to prove each (66 high-level now).

**Advanced 2026-09-27 (W-8 step 4, `mm` and `user`).** The 166 memory
requirements carry both. The rule narrowed `H.MEM.4` to what its sweep walks
-- the kernel's own root and the identity root -- and so found that O.WXN,
as stated, does not cover user mappings, which may be writable and
executable at once as on Linux (`H.MEM.18` says what the kernel does refuse
a program). Splits by the one-check rule: `H.MEM.5` (and `H.MEM.13`),
`H.MEM.7` (`H.MEM.12`, `H.MEM.17`), `H.MEM.8` (`H.MEM.16`), `H.FAIL.2`
(`H.FAIL.3`); new: `H.MEM.14`, `H.MEM.15`. 74 high-level now.

**Advanced 2026-09-27 (W-8 step 4, `smp`).** The 32 `L.smp.*` requirements
carry both. No split was forced; three high-level ones say what smp does
that nothing said -- `H.SCHED.6` (each processor finds its own record),
`H.MEM.19` (grace periods outlast readers), `H.BOOT.5` (every described
processor comes up) -- 77 high-level now. `H.FAIL.1`'s statement promises
the other processors stopped, which its criterion does not test; the split
is proposed in IMPLEMENTATION.md W-8.

**Advanced 2026-09-27 (W-8 step 4, `console`).** The 41 `L.console.*`
requirements carry both. Seven high-level ones say what the console does
that nothing said, 95 high-level now: `H.BOOT.9`, the kernel log holds no
slide and no kernel address (F-31's KASLR half, under O.ISOLATE: proposed,
since no requirement covered it); `H.TRAP.13`, a read of a log ring
(verified); `H.TRAP.14` and `H.TRAP.15`, a program's output and the log as
written; `H.SCHED.10` and `H.SCHED.11`, the console's holds with interrupts
masked and its writers' sleep; `H.FAIL.4`, a report past a held lock. The
port's lines out and bytes in refine the x86-64 slice's `H.BOOT.8`.

**Major.** They are prose doc comments (*"Forces: 1:1 kernel threads, a real
futex, per-thread TLS registers"*) explaining why the system is shaped as it
is. Excellent design rationale; not requirements with pass/fail criteria that a
test can be written against and an assessor can check.

---

## D. Tools

### F-17 — the compiler is unqualified
**Major.** `rustc 1.97.1`, pinned exactly, no unstable features in `src/kernel/` or
`src/boot/common/uefi/` — good practice, and not qualification evidence.

Ferrocene is the concrete route: a qualified Rust toolchain with evidence
packages for IEC 62304 Class C, IEC 61508 SIL 4 and ISO 26262 ASIL D. Adopting
it means pinning a Ferrocene-released rustc and checking the qualified target
list; `armv7a-none-eabi` and the three UEFI targets are the ones expected to
fall outside it.

### F-18 — six code generators produce product code and are unqualified
**Moderate.** `gen-wayland-protocol.py`, `gen-xkb-tables.py`, `gen-font.py`,
`gen-term-font.py`, `gen-panic-catalog.py` and `gen-btrfs-fixtures.py` emit
committed source. Under EN 50716 §6.7 each is class T3; under DO-330 each needs
qualification or output verification.

Mitigating: each has a `--check` mode that fails the build when its output and
its input disagree, which is the beginning of the argument.

Only `gen-panic-catalog.py` and `gen-font.py` touch the item; the rest generate
load-ring or compositor code and are out of scope at the present boundary.

**Tool operational requirements written 2026-09-25**: [TOOLS.md](TOOLS.md) §6
carries TOR-1 and TOR-2 for those two — what each shall and shall not do, its
failure mode, how it is verified, and the residual that generator and
`--check` share code so the verification is not independent. The documentation
half is done; the finding stands because a shared-code check is not
qualification.

### F-19 — the build driver and gates are unclassified
**Moderate, classified 2026-09-25.** [TOOLS.md](TOOLS.md) §3 gives every gate
and `xtask` a T1/T2/T3 class, and §6's TOR-3 covers `coverage-report.py`, the
one whose failure would be least visible — coverage is offered directly as
evidence against DO-178C table A-7 rather than used to find defects, so a tool
that over-reports produces a number nobody can distinguish from a correct one.

TOR-3 records that it has **no independent verification** and does not pretend
otherwise. Its mitigation is that both biases are declared, the residual is
enumerable, and cross-checking it against raw `objdump` is what found three
measurement defects. Qualification would need a second implementation.

That mitigation was tested on 2026-09-26 and held only partly: two more
defects were found, and the published 81.9% had both (F-10). Neither showed in
the tool's own output. They were found by comparing architectures, which
disagreed on generic code every boot runs -- a cross-check, not a second
implementation, and one the per-architecture evidence now makes routine.

The finding stands on that residual.

---

## E. Safety and security analysis

### F-20 — no hazard analysis and no risk management file
**Closed at the element level 2026-09-25** by
[SAFETY-MANUAL.md](SAFETY-MANUAL.md) §5: nine failure modes of the element
(ten since 2026-09-26, FM-10 the shootdown wait),
each with its effect at the element boundary, its detection, its mitigation and
its residual. FM-9 — kernel stack overflow with no guard page and no depth
bound — is named as the least-defended.

The earlier text on this finding was wrong in an instructive way. It said the
analysis needed a device and that a generic hazard list "would be a document,
not evidence". That is not how general-purpose kernels are certified: ISO 26262
Part 10's *safety element out of context*, EN 50716's *generic software* and
DO-178C's *reusable software component* all exist precisely so a component with
no application of its own can be analysed against **assumed** safety
requirements, with the system-level analysis exported to the integrator as an
assumption of use. QNX, PikeOS and VxWorks 653 all ship exactly this.

So the element's half is done and the system's half is exported as AoU-1 rather
than missing. What remains open is the *integrator's* risk file, which by
construction is not ours to write.

### F-21 — no Security Target
**Closed 2026-09-25** by [SECURITY-TARGET.md](SECURITY-TARGET.md): TOE
description and scope, assets, threats, assumptions, security objectives, SFRs
drawn from CC Part 2, a TOE summary specification mapping each objective to the
code and the evidence, and rationale. EAL5+ (ALC_FLR.2) claimed.

Superseded by F-21a and F-21b, which are what the ST itself records as the
reasons it would not survive evaluation.

### F-21a — no vulnerability analysis
**Closed 2026-09-25** by
[VULNERABILITY-ANALYSIS.md](VULNERABILITY-ANALYSIS.md): all seven ST threats,
attack paths enumerated per threat with the resisting mechanism, the evidence
and a verdict. Five residual vulnerabilities V-01 to V-05, superseded by F-32.

It also corrected an error in the Security Target it was written against, which
is the most useful thing it did. See F-32.

### F-32 — no SMAP, SMEP or PAN; one software check guards kernel memory
**Closed 2026-09-25**, with one honest caveat about the emulated CPU.
`CR4.SMEP` and `CR4.SMAP` are set in `init_traps` when CPUID reports them, and
secondary processors inherit them through the `CR4` snapshot
`smp::secondary_start` already copied. The boot says so:
*"cpu   ring 0 kept out of user pages: SMEP on, SMAP on"*.

**Turning it on found three real violations, and all three are in test code.**
`user/check.rs` installs an address space and reaches a user linear address on
purpose — to prove the processor walks an installed space, and to prove a
task's own space is the one installed when it runs. SMAP refused each, loudly:
a page fault at 0x50000000, then 0x30000000. They are bracketed with
`arch::permit_user_access` / `forbid_user_access`, `EFLAGS.AC` via `stac` and
`clac`, with the window kept tight around the access in the case that yields,
since `AC` is part of the context a switch carries.

**No product-code path needed one.** That is the result worth having: the claim
in `uaccess`'s header — that every legitimate access to a program's memory goes
through the direct map and never through a user linear address — is now
enforced by hardware rather than asserted, and it survived `test-boot`,
`test-threads` and `test-vfs`.

**AArch64 has PAN too**, implemented the same way: `PSTATE.PAN` set, and
`SCTLR_EL1.SPAN` *cleared* so an exception entry from user mode does not undo
it — the part that is easy to miss, since leaving SPAN set turns the protection
off for exactly the code that handles system calls. It needed no access windows
beyond the three SMAP already required, which confirms the same invariant holds
there.

Two things about it are worth recording rather than glossing.

The instruction is emitted as a word. `msr pan, #1` needs the ARMv8.1 `pan`
extension the target does not enable; `.arch_extension pan` inside an `asm!`
changes assembler state for the whole translation unit and broke section
emission, failing the link on anonymous constants; and a `const` operand to
`.inst` did the same. `0xd500419f` and `0xd500409f` are written literally, with
the derivation in a comment — the same two words Linux emits.

And the reference configuration's CPU does not have the feature. `cortex-a72`
is ARMv8.0; PAN is 8.1. The boot correctly reports *"PAN unavailable"* and
carries on. Demonstrated on a CPU that has it via `FERRIX_ARM_CPU=max`, which
prints *"PAN on"* and reaches `FERRIX-BOOT-OK`. Whether to move the Arm
reference CPU is a project decision about what every Arm test runs on, not a
certification fix, and it is left open deliberately.

**ARMv7-A cannot have it at all**: the Cortex-A7 is ARMv7-A and PAN is an
ARMv8.1 feature. There the software bound check remains the only barrier, and
V-01 stands. That is a hardware limit, not a gap that work closes.

Original text follows.

**Was:** **Major.** `uaccess.rs` says so in its own header and the code confirms it: no
`CR4.SMAP` or `CR4.SMEP` bit is set on x86-64, no `PAN` on AArch64. The bound
check in `uaccess` is the only thing between a user pointer and a read or write
of kernel memory at kernel privilege (V-01), and it bears on three of the seven
threats.

The mitigation is sound — one chokepoint, checked first, before any arithmetic
that could wrap — and it has no defence in depth. One syscall that ever
dereferences a user pointer without going through `uaccess` is an immediate
compromise; SMAP and PAN exist to make that a fault instead.

Worth recording how it was missed: an early sweep of this tree counted 56
matches for "smap" and concluded the feature was wired up. They are
`smap_base`, `smap_len` and `smap_phys` — the **s**ystem **map**. The Security
Target asserted SMAP/PAN enforcement on that basis until the vulnerability
analysis checked the registers.

*Closes when:* SMEP and SMAP are enabled on x86-64 with `stac`/`clac` around
the copy, and PAN on AArch64.

### F-31 — no side-channel or layout-randomisation defences
**Closed 2026-09-26**, both halves built behind the kernel's one build switch,
and checked by every boot. What they do not reach is carried as V-06 and in
[SPECULATION.md](SPECULATION.md) §9, not argued away.

*Was:* no Spectre, Meltdown or cache-timing analysis, and no mitigation — no
retpolines, no KPTI, no IBT or shadow stacks, no ASLR or KASLR.

*Now, the side-channel half:* analysed per architecture and built, behind the
kernel's one build switch. `cargo xtask --mitigations on`, the
default and the reference configuration, gives every program-chosen index at
the system call boundary a clamp a misprediction cannot see past (syscall
numbers, handles, descriptors, user addresses), and applies what each
processor needs and offers: on x86-64 enhanced or automatic IBRS, STIBP, SSBD,
`IBPB` and a return stack refill at each switch of address space, `VERW` on an
MDS-exposed part, a `swapgs` fence and cleared registers on entry; on AArch64
the Spectre-BHB loop, `SSBS` or firmware's workaround 2, and firmware's
workaround 1 at a switch, each decided by every core for itself so that a
machine of mixed cores (a Pixel 7's A55s, A78s and X1s) gets what each kind
needs; on ARMv7-A `BPIALL`/`ICIALLU` for the cores Arm lists
as affected, of which the reference Cortex-A7 is not one. Every processor reads
back what it wrote and the boot check fails otherwise (FX-0307); the boot log
names what is covered and what is not, on AArch64 for each kind of core. `--mitigations off` compiles all of it
out, and `cargo xtask check` builds both settings. Measured cost under KVM: +1.0%
on two million system calls, +2.8% on a thousand fork-exec-waits.

*Now, the layout half* ([SPECULATION.md](SPECULATION.md) §6.1): the loader moves
the kernel image, the direct map and the top of the vmap arena each boot, each
from its own word of `EFI_RNG_PROTOCOL`. That is 18, 16 and 17 bits on x86-64
and AArch64, and 11, 8 and 9 on ARMv7-A. The kernel is linked as a static PIE
on the 64-bit pair. On ARMv7-A, whose prebuilt `core` rules a PIE out, it is
linked with `--emit-relocs`, and the loader applies the fixups
`ferrix_elf::Elf::fixups` reads. Stage 1 refuses a kernel that is not where
the loader says, a claimed move that did not happen, and a kernel built to
move that arrived without its fixups (FX-0101). Every `test-boot` requires the
move on `on` and the fixed image on `off`. `cargo xtask test-kaslr` requires
two boots to get two layouts. x86-64 sets UMIP, without which `SIDT` would
read the IDT's address out of the image. `--mitigations off` links the fixed
static image it always was, and moves nothing.

What a processor needs and the build cannot give it — a Meltdown-affected part,
one with no IBRS form, no `IBPB`, no `SSBD`, or on x86-64 no UMIP — is excluded
by AoU-11 rather than mitigated. Retpolines were evaluated and rejected: the pinned
compiler has them only through a deprecated target feature scheduled to become
an error, and not in the precompiled `core` and `alloc`.

*The criterion, clause by clause.* KASLR exists, on all three architectures,
and the build checks it. IBT and shadow stacks are argued out: both need a
nightly compiler flag (§9). The residuals §9 lists are each built, argued or
carried:

* KPTI is argued, as not needed on the reference processors and excluded by
  AoU-11 on those that need it.
* Cache timing between processes is exported, as partitioning to the
  integrator (AoU-11).
* The libraries' clamps without `csdb`, and the tables below the system call
  boundary, are **carried** as V-06, not argued away.
* So is KASLR's own limit. Without KPTI a program with a timer can find the
  kernel, so KASLR makes an exploit need a disclosure. It does not keep the
  layout from a local timing attacker, and ASR-1 does not rest on it.

At `AVA_VAN.4`'s moderate attack potential the half built first was the half
that mattered more. Without it, isolation between processes held only
against programs that did not time their loads.

### F-34 — the direct map aliased the kernel's text writable
**Found and closed 2026-09-26.** Recorded as a finding although it never stood
open over a landing, so that O.WXN's evidence can say what the W^X sweep did
not see and when that stopped.

*Was:* **Major.** The direct map aliases every byte of RAM, the kernel image's
own frames included, and both loaders mapped all of it read-write and never
executable. The image mapping's text was read-execute and its read-only data
read-only, and the W^X sweep passed, since it asks each mapping about itself
and the alias was never executable. But a write through the alias changed the
code the image mapping runs. So O.WXN held for every mapping and the property
it exists for did not: a kernel write primitive (V-01 on Arm, or any
out-of-bounds write) could patch kernel text without making anything
executable writable. It was found by the KASLR work (F-31). KASLR moves the
direct map but not the image's physical placement, so a disclosure of the
direct map's base was enough to find the alias.

*Now:* both loaders (`src/boot/common/uefi/` and the Pixel 7 loader) cut each direct-map run
around the physical span of the image's non-writable segments
(`ferrix_bootinfo::read_only_span` and `split_run`, host-tested) and map that
span `KERNEL_RODATA`. `.data` and `.bss` stay writable in the alias: they are
writable in the image anyway, never executable in either, and the kernel's
early page tables are in `.bss` and are written through the direct map. No
kernel code writes its own text or read-only data. There are no alternatives,
static keys or text pokes, and the KASLR fixups are applied by the loaders
before the switch.

A caller could still have mapped the text writable, since `vmap::map_device`
mapped any physical address it was given, and the stage 2 device-window
checks did exactly that over the image's first page. Every interface that maps
a physical address its caller names now refuses a range touching any part of
the image, before anything is mapped (`mm::overlaps_image`):
`vmap::map_device` with `VmapError::KernelImage`, early boot's device windows
with `EarlyError::KernelImage`, and a user space's device and GPU-window
mappings (`AddressSpace::map_device`, `map_window`) with `SpaceError::Refused`,
whatever aperture or window the caller holds. The whole image, not only its
text: its data is RAM the kernel uses cacheably, and no device has registers
in it. The device-window checks now use a frame they allocate.

*Checked by the build:* after the W^X sweep, every boot walks the kernel's
tables for every mapping of the text's and read-only data's frames. It fails
with FX-0204 if one is writable, or if the direct map does not alias the whole
span (*"sealed 4416 KiB of text and read-only data, 1697 mappings of it,
none writable"* on x86-64). A loader with the cut but without the seal was
refused by name. A write of one byte of text through `mm::direct_map` faults
on all three architectures with FX-9001 (*"page fault at … (kernel write,
protection)"*). Both results are quoted in the commit that added the sweep.
Each boot also asks for the refused windows and requires the refusal: stage 1
an early window over the text, stage 2 `vmap::map_device` over the text's
first bytes, a range running into the image from below, a page in its middle
and its last byte, and stage 6 a user device mapping and a GPU window over it.
With each refusal removed in turn (scratch), x86-64 stops at *"stage 1
self-check failed: an early device window over the kernel's text was
mapped"*, *"stage 2 self-check failed: a device window over the kernel image
was mapped"* and *"stage 6 self-check failed: a user device mapping of the
kernel's image was not refused"*.
The cost is 1022 more 4 KiB leaves on each architecture: the image sits on a
page boundary, so each edge of the span breaks one 2 MiB block. Boot time
under KVM did not change measurably.

*Not reached:* the image's physical placement is still fixed (V-06), so a
disclosure of the direct map's base still gives the alias of the image's data,
which is writable in its own mapping too.

### F-36 — a user page table went back before the shootdown that covered it
**Found and closed 2026-09-26.** Recorded as a finding although it never stood
open over a landing after it was seen, so that ASR-1's evidence can say what
the shootdown order did not cover and when that stopped.

*Was:* **Major.** `user/space.rs` states the order a translation comes down
in: out of the tables under the space's lock, the set of processors that may
hold it read, the shootdown sent and every answer waited for, and only then is
anything the translation reached given back. The leaf frames followed it.
The page tables did not: `mm::unmap_in` freed each table the moment an unmap
emptied it, in its unmap callback, which is step one. A processor caches the
walk as well as the leaf -- x86-64's paging-structure caches and the Arm walk
caches keep "the table for this 2 MiB is at frame F" -- and clearing the
parent's descriptor in memory does not reach them; only the invalidation does.
So between the unmap and the shootdown's last answer, another processor running
the same space could walk *through a table already back in the allocator*. A
frame handed out in that window to another thread of the program, as a page
it could fill with descriptors of its own choosing, would be read by that walk
as a table, and translate to any physical memory with the user bit set. An
isolation break in the core, reached by `munmap`, `mprotect`, `mremap`,
`madvise`, `fork`'s copy-on-write takedown and a VMO taking its pages back. The
kernel's own unmaps were never affected: `unmap_kernel_all` held its tables
until after `flush_tlb_everywhere`. IOMMU domains had the same order:
`unmap_io` freed a table before the unit's invalidation, and VT-d's
paging-structure caches and the SMMU's walk cache would walk it for a device.
It was reported by the memory coverage work and confirmed by reading.

*Now:* `mm::unmap_in` takes the shootdown's own `TlbPages` and adds to it the
range and every table it emptied, unlinked but still allocated
(`mm/unlinked.rs`), and `smp::flush_tlb_pages` gives the tables back only after
every processor it reached has answered. The list takes no memory, since an
unmap cannot report running out (F-23): it is threaded through the emptied
tables' own first descriptors, and a page-aligned address has its low bits
clear, which no encoding -- x86-64, VT-d, Arm long or short descriptors, stage
1 or 2 -- reads as valid. A domain's unpin, and a failed pin's rollback,
release the tables after the unit's flush has completed, and keep them if it
never did. A list dropped unreleased keeps its tables for good and counts them.
Trees no processor can walk -- a dropped address space, and the secondaries'
bring-up trees, which each core left with a full TLB flush -- still free at
once, through `mm::unmap_unwalked`. The `mm::prune_in` the memory coverage
work adds, which gives back the empty tables a failed map leaves, runs only
as a space is dropped and is right to free at once for the same reason.

*Checked by the build:* stage 4 maps and unmaps a page in a fresh tree and
requires every table the unmap emptied to be still allocated and not counted
given back when it returns, and the shootdown to give back exactly those.
With the old callback put back (scratch), x86-64 `--smp 2` stops at
*"stage 4 self-check failed: an unmap gave back the tables it emptied before
its shootdown"*. Stage 6 requires that no unmap's tables were ever kept for
want of a shootdown. What the build cannot show is a walk landing in the
window: under TCG QEMU caches no intermediate walk at all, and under KVM the
window is a few microseconds wide and the freed frame must be reused and
filled in it. The fix rests on the ordering rule it restores, which the check
holds.

### F-21b — the TOE claims no audit and no authentication
**Closed 2026-09-27**, both halves.

*The FAU half* by [AUDIT.md](AUDIT.md), built in three slices: the store
(two rings of static storage, gapless numbering and a lost count, fairness
per budget, the boot's own records pinned), a record at each decision the
TSF makes, and the reader -- a read-only capability only pid 1 holds,
`audit_read`, init keeping the record on the volume, and nothing before a
power action lost without a line on the console. The Security Target claims
FAU_GEN.1, FAU_GEN.2 (refined to the TSF's own subjects), FAU_SAR.1,
FAU_SAR.2, FAU_STG.1 and FAU_STG.4 (overwrite the oldest, counted and
reported; not .3) under
O.AUDIT, for P.ACCOUNTABILITY, and O.AUDIT's thirteen `H.AUD` requirements
are each verified by a check -- boot checks, `test-init` and a
`ferrix.checks=skip` boot read from outside -- with a negative control for
every recording site. A record costs 21 ns under KVM (MEMORY-AND-TIMING
§1.7). The records once on disk are OE.AUDIT_STORE's, and a DMA fault after
boot is recorded only when something reads the unit, which the ST states.

*The FIA half* by the environment: OE.AUTH and A.AUTH (1cd1806f), `authd`
identifying and authenticating people and keeping the log of it outside the
TOE.

*Was:* **Moderate.** There is no FAU family at all, and FIA lives in the
uncertified load ring. Defensible for an isolation kernel and the reason no
OS Protection Profile can be claimed — but an evaluator would press on
whether a TOE that cannot record a security-relevant event can claim EAL5.

*Would close its FAU half:* the design in [AUDIT.md](AUDIT.md), reviewed by
this review and written 2026-09-27, not built: the TSF's own decisions
recorded at their choke points in two fixed rings -- one for grants, changes,
ends and system events that refusals cannot evict, one for refusals with
per-job fairness -- read only through a capability pid 1 is given, with the
boot's configuration, `ferrix.checks=skip` included, among the records. Open
until it is built with the checks its §6 names. FIA stays the
personality's.

*Advanced 2026-09-27:* the store and the start-up and configuration records
are built and checked ("Keep an audit record of what the kernel decides:
the store"); the event call sites and the ST claims follow. Not claimed.

### F-35 — the job quotas FRU_RSA.1 claims are not built
**Closed 2026-09-26** by work order W-13 ([IMPLEMENTATION.md](IMPLEMENTATION.md)),
with `FRU_RSA.1` refined in the Security Target to what the quotas bound.
What they left out was F-37, closed the same day by charging the kernel heap
a job's programs hold to its memory limit.

*Was:* **Major.** The Security Target claimed `FRU_RSA.1`: the TSF enforces
maximum quotas of physical memory, kernel objects and CPU time that a job can
use simultaneously, for O.QUOTA against T.EXHAUST, and the vulnerability
analysis credited "job quotas" with resisting its first two T.EXHAUST paths.
None of the three existed. Nothing charged a job for frames or heap, cgroupfs
built no controller, `object/job.rs` limited only the job tree's depth and
descendants, nothing limited the processes a job made but the global
`PID_MAX`, and EEVDF shared the processor per task, so a job with *n*
runnable tasks took *n* shares. T.EXHAUST was *not resisted* but for CPU per
task.

*Now:* every job below the tree's root has a quota slot in a table of atomics
(`src/kernel/src/object/quota.rs`), charged hierarchically -- a limit anywhere
above refuses, and a refused charge takes nothing -- and set through a job
handle (`job_set_limit`, `job_get_quota`) or cgroupfs:

* **Tasks** (`pids`): a process and each thread beside its first, charged
  as it is made, moved with a process, uncharged at reap. `fork` answers
  `EAGAIN` at the limit, `process_create` `SHOULD_WAIT`.
* **Memory** (`memory`): every frame of a program's memory -- anonymous and
  file pages, copy-on-write copies, the page cache's fill -- and the page
  tables of its address spaces, charged to the job of the task that caused
  it and uncharged wherever the frame is freed, from a slot index the frame
  record keeps. Refused as running out is refused: `ENOMEM`, the fault's
  signal, `NO_MEMORY`.
* **Kernel objects**: VMOs, channel ends, ports and jobs a program made,
  charged for as long as each exists wherever it went. Refused `NO_MEMORY`.
* **Processor**: a weight per job (`cpu.weight`) applied to each task's
  weight, so a job's share of a contended processor no longer grows with its
  task count. A share, not a cap: `FRU_RSA.1` now says so.

The evidence is the `quota` line of every boot, on all three architectures:
a fork loop refused at exactly its job's 8 tasks, faults refused at exactly
48 pages with a sibling job faulting on, objects at 5, one task alone in its
job keeping 50.0% of a processor against eight spinning in another (11.1%
with the job share taken out, the negative control), and every counter and
slot back at zero after. The `cgroups` line and a `test-vfs` command drive
`pids.max`, `memory.max` and `cpu.weight` as a Linux program does. Four
negative controls fail by the checks' own messages (W-13). Charging costs a
fault 848 ns against 832 on main and a fork 79 µs against 77, under KVM,
within the runs' spread.

### F-37 — the kernel heap a job drives through the Linux personality is not bounded
**Closed 2026-09-26** by work order W-15 ([IMPLEMENTATION.md](IMPLEMENTATION.md)),
with `FRU_RSA.1`'s memory refined to count the kernel heap a job's programs
hold beside their frames.

*Was:* **Moderate.** Opened 2026-09-26, as F-35 closed. The quotas F-35
built bound a job's user memory and page tables, its native objects, its
tasks and its share of a processor. What a job held of the kernel heap
*through the Linux personality* was charged to no job: the regions of its
address spaces (a shared mapping of a file makes a region and no VMO), the
files and names it made in a memory filesystem, descriptors in flight in a
Unix socket's queue, and the sockets, pipes and event files a descriptor
holds, which `RLIMIT_NOFILE` and the task limit bounded only as a product.
All of it was bounded by the machine's memory, and running out of it was
reported, not fatal (F-23) -- but not bounded per job, and the load's own
allocations, which share the heap, stop the machine when it is gone (V-05).

*Now:* the job's memory counter is in bytes, and the kernel heap its
programs hold is charged to it beside their frames, against the one limit
-- as cgroup v2 folds `kmem` into `memory.max`. A charge is a token from
`src/lib/kernel/kmem` made where the allocation is, to the job of the task whose call
made it, and kept inside what it pays for, so every path that frees the
object frees the charge. A job at its limit is refused the object with
`ENOMEM` before anything changes. An audit of every allocation in the load
ring and its libraries that a program can make and keep found thirteen
kinds, and each is charged:

* open file descriptions; dentries, including cached misses; the location
  and mount of each pipe, socket or event file; mounts;
* tmpfs inodes, names, symbolic links and instances, and a file's page
  store (its pages are frames, charged to whoever writes them, as before);
* pipes and their buffers as they grow; `AF_UNIX` sockets, their backlogs,
  and each message in a queue -- a message carrying descriptors is its
  sender's, as Linux charges it, and each descriptor stays its opener's
  however long it is in flight;
* epoll sets and registrations; eventfds, timerfds and signalfds;
* descriptor tables as they grow, so `dup2` far up is charged to there;
* regions of an address space, to the job that made the space, and each
  name a space gives an object -- what bounds a shared mapping of a file;
* record locks, any number of which one description could set;
* System V semaphore sets, to the job that made them until `IPC_RMID`
  (they outlive their maker, as Linux's charge to the maker's memory
  cgroup does), each process's `SEM_UNDO` records to its job, and each
  blocked `semop`'s record to its caller's job, given back on every way the
  wait ends (added 2026-09-28, the fourteenth kind; `syscall/sem.rs`). The
  set ids are the system's, so a job may also hold at most 32,000 sets
  (Linux's `SEMMNI`, per job) in an id space of 2^24, and a job at that
  bound leaves its siblings free to make theirs;
* System V shared memory segments, each segment's record to the job that
  made it and its memory object to that job's object count, until it is
  taken out -- by `IPC_RMID` with nothing attached, or at its last detach
  after one -- so it outlives its maker as on Linux (added 2026-10-02, the
  fifteenth kind; `syscall/shm.rs`). Its pages are frames, charged to
  whoever touches them. The ids are the namespace's, so a job may hold at
  most 4,096 segments (Linux's `SHMMNI`, per job) in an id space of 2^24;
  and since a segment reserves pages that no limit sees until they are
  touched, a job may reserve at most 4 GiB of them together, a quarter of
  the namespace's 16 GiB `SHMALL`, so a job at either bound leaves its
  siblings free to make theirs (both shown by the boot's `shm` line);
* `/proc`, sysfs and cgroupfs snapshots, rendered at open;
* internet sockets, a TCP connection's queues and reassembly (to the job
  that made it, or its listener's), and datagram, packet and netlink
  queues. What arrives for a job at its limit is dropped, as at a full
  queue.

Five things the audit found were not missing charges but leaks or missing
checks, and were fixed: a closed TCP listener leaked the connections it had
not accepted; a process's task list was never pruned; netlink changed
routes and addresses without privilege; an empty datagram counted nothing
against its socket's capacity; and a btrfs root held changed tree nodes
uncounted until the commit interval.

*Argued rather than charged,* each with what bounds it: one per task and so
bounded by the task limit -- a kernel stack, a futex waiter, signal state, a
process's recorded program and arguments (at most 256 KiB); fixed by the
machine and shared by every job -- 256 pseudoterminal pairs, the neighbour
and IP reassembly caches, the routing tables (now root's alone), a btrfs
transaction's changed nodes (at most the commit threshold); a few dozen
bytes of bookkeeping per charged 4 KiB frame; and one whole-file lock holder
per charged open description. The dentry cache keeps up to 4,096 dentries
nobody holds, charged to whoever looked them up; a job whose limit they take
is refused where Linux would reclaim them, which is M1's rest
(`docs/CGROUPS.md`).

*Evidence:* every boot's `kmem` line fills a job at a 32 KiB limit with each
kind in turn until it is refused, sees a sibling at the same limit make one,
and requires every byte back: *"at a 32 KiB memory limit a job made 34
files, 14 pipes, 5 socket pairs, 70 descriptors in flight, 128 epoll
registrations, 31 eventfds, 255 regions of one mapping and 454 record locks,
and was refused one more of each -- ENOMEM, ENOLCK for a lock -- while a
sibling made one; every byte of heap charged came back"* (x86-64,
2026-09-26); FX-0906 otherwise. `test-vfs` command 21 fills `/tmp` from a
shell whose `memory.max` leaves it 256 KiB and reads `memory.current`,
`memory.stat` and `memory.events` as a program would. The libraries' host
tests require the same of each kind against a recording account. Four
negative controls, scratch, each stop the boot by its own message (W-15).
Cost under KVM: an open and close 60 ns (1.5%) dearer in the root job and
136 ns (3.4%) in a limited one; a 64-byte pipe write and read unchanged; a
tmpfs create, 4 KiB write and unlink 4.0% and 4.8% dearer.

### F-38 — a device can write a dead driver's frames after the kernel gave them back
**Found and closed 2026-09-26** (found by the audio work, ferrix-90).

*Was:* **Moderate.** `object/pin.rs` gives a pinned buffer's frames back the moment the pin closes.
The order it closes in is right for the hardware the claim names: the domain's
unpin takes the translation out and waits for the unit's invalidation to
*complete* -- VT-d's invalidation-wait descriptor, the SMMUv3's `TLBI` and
`CMD_SYNC` -- before the frames go back, so a device can no longer reach them.
QEMU's device models do not all go through that translation on every access:
virtio-snd takes a host mapping of a buffer when it pops it from the queue and
writes a returned buffer's status through that mapping for up to about 160 ms
after the kernel has unpinned it, and its reset does not stop its streams. So
under the reference machine a sound driver that dies mid-stream has its
device write into frames the allocator has already handed on: the x86-64 TCG
gate's DMA-fault check caught the writes, and a restarted driver on AArch64
died of `SIGILL` in the frames it was given. On ARMv7-A, whose domains are not
translated, pinned frames are kept and the hazard does not arise. This bears
on T.DMA ([VULNERABILITY-ANALYSIS.md](VULNERABILITY-ANALYSIS.md)): the
objective holds on hardware whose devices honour the invalidation and is not
met for this device model under the emulator the reference configuration
names.

*Now:* a pin closed because the process that made it died is not given back:
it stays mapped in its device's domain, its frames held, one reference each,
and moved off the dead job's memory charge to nobody's (`mm::disown_frame`), so
that job's counters come back to zero. What the device writes late lands in the
dead driver's own pages, still mapped, and neither faults nor reaches a frame
anything else holds. The quarantine is released on an event, not a timer: the
device's core accepting the next driver's `HELLO` (`object::pin::
quarantine_release`, called by the audio and display cores), which that driver
sends only after resetting the device and, for virtio-snd, releasing every
stream with no transmit queue enabled. The release unpins as any pin is, the
invalidation completed first, and only then gives the frames to the allocator.
The record a quarantine needs is allocated with the pin, so a close allocates
nothing (F-23). A device no driver takes up again keeps its quarantine, as an
untranslated domain keeps its pins. The kernel bounds the quarantine itself
rather than rely on devmgr to stop restarting: a domain whose quarantine holds
`QUARANTINE_CAP_PAGES` (two drivers' worst case, the display card's 65536
pages and 1024 more, each) refuses the next pin for its device with
`QUARANTINE_FULL` until a release. A frame whose reference cannot be taken is
kept for good, never given back. A pin closed by a live driver is given back
as before. Address translation services are never enabled, and enumeration
switches off any firmware left on (SAFETY-MANUAL AoU-12). With the quarantine
in, devmgr starts a sound driver again as it does a display driver.

*Checked by the build:* a stage 10 boot check drives a translated domain's
quarantine to a cap of one page, requires a dead process's pin to be
quarantined, the next pin to be refused, one to be taken again after the
release, and a live process's pin to be given back at once. `cargo xtask
test-audio`'s restart boot, on all three architectures: `snd` killed twice under a running stream, the card back each
time, and a second played whole on the third driver's card. On x86-64 and
AArch64 it requires each death's pins to be quarantined and released, and the
device to have written at least one quarantined page after its driver died,
which the release counts by the pages' contents: the writes that before
landed in frames handed on. The x86-64 run's DMA-fault check is clean. With the
quarantine switched off the AArch64 restart died of `SIGILL`; the fault check
alone is not a reliable negative control, since QEMU's late status writes go
through the mapping it took at pop and do not fault.

### F-39 — a native process made by any user ran as root
**Found and closed 2026-09-26** (f84a8d3c, found by the init work, ferrix-15).

*Was:* **Major.** `process_create` loaded its child through `Process::new`,
which gives a process root's credentials, and nothing replaced them. It asks
for `MANAGE` on the job the child goes into, and `job_for_cgroup` grants
`MANAGE` on a cgroup's job to whoever may write that cgroup's `cgroup.procs` --
which a delegated cgroup's owner may, by design. So a uid-1000 service with
`Delegate=yes` could make a VMO, create a process in its own cgroup's job and
have it run as root: a program gaining authority its creator never held, which
T.FORGE's paths had not considered.

*Now:* the native loader takes the creator, and the child gets a copy of its
credentials, as a fork child does, set before the process is registered, so
nothing ever sees it as root. Only devmgr, which the kernel makes for itself,
has no creator and is root's; a creator of another personality's is refused
rather than given root.

*Checked by the build:* stage 13's cgroup check (FX-1301,
`fs/cgroupfs/creator_check.rs`) delegates a cgroup to uid 1000, has a uid-1000
process take `MANAGE` through `job_for_cgroup` and create a process there, and
requires the child's ids to equal its creator's and the child, run, to answer
`getuid` with 1000. Both negative controls -- the old root credentials, and
the ids comparison skipped to show the `getuid` half alone -- stop the boot,
and are quoted in the commit.

### F-40 — a delegated job can lift its own limits
**Found and closed 2026-09-26.** Found by the init work (ferrix-15) while
checking this review's condition on delegation, read from the code, and
closed there with the fix this review designed with it.

*Was:* **Moderate.** The native `job_set_limit` asks only for `MANAGE` on the job
(`syscall/native.rs`). A delegated cgroup's owner gets `MANAGE` on that
cgroup's own job through `job_for_cgroup`, as F-39 describes, so a
`Delegate=yes` unit running as uid 1000 under `MemoryMax=16M` can set its own
job's memory limit to unlimited, and its task limit likewise -- around the
root-owned `memory.max` and `pids.max` files that are how Linux keeps a
delegatee from raising its own limits. Every ancestor's limit still binds,
since a charge walks every ancestor (F-35), so the machine stays bounded by
whatever the delegating job was given; what fails is `FRU_RSA.1` for the
delegated job itself, the bound its unit file asked for.

*Now:* a job right of its own, `SET_LIMIT` (`1 << 7` in
`src/lib/proto/native-abi`), which `job_set_limit` requires instead of `MANAGE`
(`syscall/native.rs`). `Rights::JOB` carries it, so a job `job_create` makes
and the handles root and init hold have it. `job_for_cgroup` grants it only
with `MANAGE`, and only when the caller may also write the cgroup's
`memory.max`, `pids.max` and `cpu.weight` (`fs/cgroupfs.rs`,
`limit_metadata`), which a delegation by `chown` leaves root's. Like every
right it is dropped by duplication or transfer and never regained. A child's
limit may be set above its parent's as a number and binds nothing beyond it,
the charge walking every ancestor (F-35); `docs/CGROUPS.md` §5 says so.

*Checked by the build:* stage 13's `limits` check (`fs/cgroupfs/limits_check.rs`)
delegates `/check-l` to uid 1000 under a 16 MiB `memory.max` and has the
delegatee refused nine ways: `job_for_cgroup` asked for `SET_LIMIT`,
`job_set_limit` on its own job for memory, tasks and processor weight, a
duplicate asked for `SET_LIMIT` and `job_set_limit` through a plain
duplicate, each `ACCESS_DENIED`; and opening its own `memory.max`,
`pids.max` and `cpu.weight` for writing, each `EACCES`. `memory.max` still
reads 16 MiB, and a limit on a job the delegatee made itself is accepted.
Two negative controls (scratch, x86-64): `job_set_limit` on `MANAGE` again
stops the boot at *"a delegatee raised its own cgroup's memory limit with
job_set_limit"*, and `job_for_cgroup` granting `SET_LIMIT` to any writer of
`cgroup.procs` at *"job_for_cgroup gave SET_LIMIT to a delegatee that may not
write the limit files"*.


### F-41 — a page mprotect made writable after a fork wrote into the other process's copy
**Found and closed 2026-09-27** (c9661d5c, with its boot check 469bf17c; found
by the Pixel 7 work, ferrix-d4, as a Chromium zygote's "stack smashing
detected").

*Was:* **Major.** A private file mapping's page is copied into an anonymous
frame of its own on its first write (the shadow page), and `fork` leaves that
frame shared by parent and child, read only, to be copied on the next write.
The copy-on-write mark was set only for pages writable at the fork. A page
written before the fork and then made read only -- `ld.so`'s RELRO after
relocation is exactly that -- was shared unmarked, so when one process made it
writable again with `mprotect` its write went into the frame the other still
mapped. One process changed another's memory, a T.MEMORY path the analysis had
not considered: here, the child corrupting the parent's
`__stack_chk_guard`. The file's own page-cache page was never reached: a
private file mapping maps the file's frame read only and copies on the first
write, and the file still read its original bytes when traced.

*Now:* `mprotect` making a private region writable marks it copy-on-write
(`src/lib/kernel/vma`'s `protect`), merging and splitting regions keep the mark,
and the next write gives the writer a copy of its own.

*Checked by the build:* stage 6 (`user/check.rs`) writes a private page, makes
it read only, forks, and has the child make it writable and write through its
own translation: the child must get a copy, the parent's frame must keep its
value, and no frame may leak. With `protect` not marking the region
(scratch, AArch64) the boot stops at *"a write to a page a fork left read-only
and mprotect made writable reached the other process's page"*. A host test in
`src/lib/kernel/vma` holds the mark itself.
### F-42 — a write refused for memory closed the handles it carried
**Found and closed 2026-09-27** (found by the F-10 coverage work on
`object/channel.rs`, ferrix-90: the arm `Endpoint::write` called
unreachable was reached).

*Was:* **Minor.** `channel_write` promises that the handles a message
carries leave the process only if the write succeeds. `Endpoint::write`
checked the peer's queue for size and room, took the handles out of the
sender's table, and then pushed the message, and the push grows the queue
fallibly. A queue that had never held a message had no room, so a write into
it with no memory for that growth was refused with `NO_MEMORY` after the
handles had left the sender: the arm said to be unreachable closed them. A
program under memory exhaustion lost the capabilities it had tried to send,
with no way to know, and its handle numbers named nothing afterwards.

*Now:* the queue's room is made (`MessageQueue::reserve`) under the same lock
before the handles are taken, so a write refused for memory has taken
nothing, and the push after it cannot be refused for memory.

*Checked by the build:* the native refusal check (`syscall/native_check.rs`,
`a_write_without_memory_keeps_its_handles`) writes a message carrying a
handle into a fresh channel with each of the write's first allocations
failed in turn, and requires every `NO_MEMORY` to leave the handle in the
sender's table and nothing queued. A host test in `src/lib/kernel/objects`
holds `reserve` itself.


### F-43 — O.WXN and ASR-2 claim every mapping is W^X; a program's own need not be
**Moderate.** Open. Found 2026-09-27 by the W-8 memory slice, writing the
low-level requirements for `user/space.rs`.

O.WXN says *"Ensure no mapping is both writable and executable"*, ASR-2 *"No
mapping shall be simultaneously writable and executable"*, and FM-2's detection
says every boot sweeps all mappings. The kernel keeps that for its own
mappings: every kernel mapping is W^X at map time, the stage 2 sweep walks the
kernel's root and the identity root, and the sealed sweep holds the image's
text read only through every alias (F-34). A program's mappings are another
matter: `mmap` and `mprotect` pass `PROT_WRITE|PROT_EXEC` through
(`syscall/memory.rs::protection`, `user/space.rs::user_page`), as Linux does,
so a program may map a page it can both write and run -- what a JIT that does
not flip permissions relies on. The sweep never walks a user root, so nothing
checks this either way. What the kernel does refuse a program -- an executable
device window, an executable native `vmo_map`, a writable vDSO data page -- is
H.MEM.18, not yet verified by one check.

So the claim is broader than the enforcement. It is not an escalation
(T.ESCALATE is about ring 0 running attacker-chosen code, and a user RWX page
runs at user privilege), but an assessor reading "no mapping" finds the
counter-example in one `mmap` call.

*Would close it* -- the customer's decision, one of:
1. **Enforce W^X for programs too**: refuse `PROT_WRITE|PROT_EXEC` together
   (EACCES, as SELinux's `execmem` denial does), extend the sweep to user
   roots, and accept that a JIT which needs RWX pages must flip permissions.
   Chrome's V8 and most modern JITs already do; some do not.
2. **Narrow the claim**: O.WXN and ASR-2 say "no kernel mapping", FM-2's
   detection says what the sweep walks, and an assumption of use tells the
   integrator that programs may map RWX memory unless a policy forbids it.
3. **Both, switchable**: a build or boot option, with the reference
   configuration naming which one is certified.

A second way a program gets such a mapping, since 2026-10-02: `shmat` with
`SHM_EXEC` and without `SHM_RDONLY` maps a System V segment shared,
writable and executable, as Linux does (`syscall/shm.rs::shmat`); option 1
would refuse that combination too.

### F-47 — the Security Target had no SFR dependency rationale
**Found and closed 2026-09-27** (found by the certification review of the
audit slice 3b, which claimed FAU_GEN.2 and so FIA_UID.1's absence).

*Was:* **Moderate.** ASE_REQ asks that every dependency of every SFR be met
or its absence justified. The ST claimed seventeen SFRs and said nothing
about their dependencies. Three were unmet and unjustified: FAU_GEN.2's
FIA_UID.1, and FMT_SMR.1 and FMT_SMF.1, which FMT_MSA.1 and FMT_MSA.3 depend
on. An evaluator stops at the first.

*Now:* SECURITY-TARGET.md §8.4 tabulates each SFR's dependencies from CC
Part 2 and marks each met or not. The three unmet ones are justified: people
are identified in the environment (OE.AUTH) and FAU_GEN.2 names the TSF's own
subjects; the TOE has no roles, because authority is a handle's rights. The
management functions FMT_SMF.1 would list are named, and claiming them (and
FMT_MTD.1 for job limits) is left to the ST owner as a backlog row.

*Checked by:* the table, read against CC Part 2's dependency lists; this is a
document finding, and the document is what changed.

*Left open, as F-52:* the conformance claim names no CC version.

### F-52 — the conformance claim names no CC version
**Moderate.** Open. Found 2026-09-27 by the certification review, writing
F-47's dependency table.

SECURITY-TARGET.md §2.1 says *"CC Part 2 conformant, CC Part 3 conformant,
EAL5 augmented with ALC_FLR.2"* and names no version of the Common
Criteria. ASE_CCL asks for one. The SFRs' names, their operations and
§8.4's dependencies are written from CC 3.1 revision 5's Part 2. Under the
CCRA's transition policy for CC:2022, a new evaluation may no longer start
against CC 3.1 R5, and CC:2022 restructured several families this ST uses,
FAU_STG among them.

*Would close it:* §2.1 names the version the evaluation will use (CC:2022
revision 1, unless the scheme says otherwise), and every SFR in §5 and every
row of §8.4 is re-read against that version's Part 2, renamed where it
renumbered a component. This is the ST owner's call; the review can do the
re-reading once the version is chosen.
### F-22 — no safety case
**Closed at the element level 2026-09-25** by
[SAFETY-MANUAL.md](SAFETY-MANUAL.md): the argument is §2 (assumed safety
requirements, with the evidence for each), §3 (the safe state, and the
obligation it creates), §4 (eleven assumptions of use) and §5 (the failure
analysis).

The generic application conditions EN 50716 asks for are AoU-1 to AoU-11, and
several of them exist *because* a finding is open — no WCET (F-24), no bound
on memory and a load whose allocation failure is fatal (AoU-5, F-23 closed for
the item itself), reduced claims on ARMv7-A (F-32, V-03), processors the side-channel defences do not cover (F-31). Those stop being embarrassments and become stated conditions the
integrator designs around, which is what an application condition is for.

What remains is assessment by somebody independent, which is F-27 and not
this.

### F-23 — dynamic memory allocation throughout, with no bounded-allocation argument
**Closed 2026-09-26** for what it measured: allocation failure in the item is
reported, not fatal. The bound it also names is not claimed, and is exported
as AoU-5. [MEMORY-AND-TIMING.md](MEMORY-AND-TIMING.md) §1 has the design.

*Was:* **Major.** 225 allocation sites across 40 files in the item's product
code, and every one fatal. `KernelAllocator::alloc` returns null on failure,
and there is no `#[alloc_error_handler]` in the tree, so a `Box::new`, `Arc::new`,
`Vec::push` or map insert that met an empty heap reached Rust's default
handler and aborted. `src/lib/kernel/heap` reported `OutOfMemory` properly, and the
`GlobalAlloc` adapter above it threw the distinction away. The obvious fix is
unavailable: `#[alloc_error_handler]`, `Box::try_new`, `Arc::try_new` and
`BTreeMap::try_insert` are unstable, verified against the pinned 1.97.1, and
`src/kernel/` uses no unstable features. Only `Vec::try_reserve` is stable.

The count also missed the worst of it. The scheduler allocated a tree node on
every enqueue, and so allocated from interrupt context and with the run queue
locked: a wake-up from an interrupt handler could stop the machine.

*Now:* fallible construction built from stable parts, used at every site.

* `src/lib/kernel/fallible` makes `Box`, `Vec`, `VecDeque` and `String` fallible:
  `try_box` allocates through the global allocator and builds the box with
  `Box::from_raw`, which `Box`'s documentation makes part of its contract,
  and the rest reserve with `try_reserve` before they grow. Host-tested
  against a recording allocator, and run under Miri in CI.
* `Arc` and the ordered maps allocate inside `alloc` with layouts it does not
  publish, and cannot be made fallible. They run in a *reserved section*
  (`src/kernel/src/mm/reserve.rs`), which first fills this processor's reserve to
  16 objects of every size class and fails there, before anything starts. A
  heap refusal inside the section is then served from the reserve. The depth
  argument is in the module: one `Arc` is one allocation of a layout
  `ferrix_fallible` computes and tests, and a map insert is at most height + 2
  nodes, which the host tests measure against the pinned standard library.
* Every site in the item uses one or the other, and returns `NO_MEMORY` from a
  native call, `ENOMEM` from a Linux one, `EAGAIN` from `madvise`, or a
  refused step at bring-up. Each task lends the scheduler its own queue nodes,
  so queueing, picking and waking allocate nothing
  (`src/lib/kernel/sched/tests/no_allocation.rs`).
  Taking pages out of an object needs no memory once its list has room. A
  decommit that cannot be refused falls back to 32 pages at a time from the
  stack, and asks the spaces that map the object one at a time when there is
  no memory to list them.
* Bring-up's allocations are fatal by design. 73 sites, all before the first
  program runs or when a processor comes up, are marked `FATAL-ALLOC:`, and
  an allocation failure before the boot completes names itself: FX-0007. One
  after it can only be the load's, whose allocations stay infallible, and it
  is FX-0008.

*Checked by the build:* `tools/common/check/check-fallible-alloc.py`, the "fallible
allocation" step of `cargo xtask check`, finds every call to an allocating
standard-library API in the item's product code and fails on one that is not
argued. `NOALLOC:` says the call cannot allocate: room was reserved, or the
type only looks like a collection. `FALLIBLE:` names a fallible call the
pattern cannot tell apart, and `FATAL-ALLOC:` marks a bring-up site. It
read 0 unmarked, 31 `NOALLOC`, 13 `FALLIBLE` and 73 `FATAL-ALLOC` in the item,
with an empty ratchet baseline, `tools/common/data/fallible-alloc-baseline.json`.

**The hole, found the same day.** `syscall::signal::Signals::default` built
its tables with `vec![..; NSIG]`, and so did the `Queue` under every thread.
A refused frame there stopped the kernel on any `fork` or `clone` -- and on
`process_create` and `process_start`, item calls that make a POSIX process
and its first thread through the load. The gate never saw it: `signal.rs` is
a load file, and the item never names `Signals`; it calls a load hook that
does. Nor could the negative control, whose injection fails only the fallible
calls. The tables are now made by constructors that answer `AllocError`, with
no `Default` or `Clone` that allocates, and stage 7 makes each with every
allocation failing and requires the error (negative control: the old `vec!`
put back stops the boot there). The gate now also reads the load files those
two calls run to make a process and a thread -- `signal.rs`, `thread.rs` and
`process.rs` -- finds a `Default` that allocates in any kernel file and flags
a call of it, flags a derived `Clone` over an owned heap field, and knows the
turbofish and `default`/`make_mut` forms; each in its self-test, and together
they flag the old `signal.rs` at nine sites. It read 14 unmarked, all in
`process.rs` and recorded in the baseline as debt that may only fall: fork's
copies, the task and thread lists, the program name. It reads 13 since
F-37's audit put the task list's two pushes behind one that prunes it. What else those calls
reach in the load, and what still stops the kernel there, is listed in
[MEMORY-AND-TIMING.md](MEMORY-AND-TIMING.md) §1.3. F-23 stays closed for what
it measured, the item's own source; the claim that no allocation a program
can reach stops the machine was wrong by these, and is narrowed. Every boot
runs the negative control (`object/alloc_check.rs`, stage 9, FX-0902), which
fails every *n*th allocation of one process, for six periods, while it drives
the native ABI. Every call must succeed or answer `NO_MEMORY`, and nothing may
leak. A section must complete on the reserve alone with the heap refusing. A
decommit of a mapped object must give back every page with every allocation
failing. It reads *"486 native calls with 162 allocations failed under them:
150 answered NO_MEMORY, the rest succeeded, nothing leaked; 35 allocations
served from a reserve; 80 pages decommitted with none"*, the same on all three
architectures.

*What it does not claim.* No bound on what the item allocates: a program
still drives the kernel heap, and V-05 stood for the paths with no quota,
until F-37 charged them to the job. No
recovery for the load: its allocations still stop the machine (AoU-5).

The gate's docstring lists what it cannot see, and each was checked by other
means:

* **Allocation inside a library the item calls.** `src/lib/kernel/vma`, `src/lib/kernel/objects`,
  `src/lib/kernel/sched` and `src/lib/kernel/sync` were converted with the item. The other
  libraries allocate nothing on the item's paths, except behind the load's
  interfaces, where the allocation is the load's.
* **`.clone()` of a collection.** The item's 18 clones were audited by hand,
  and none allocates.
* **Conversions that allocate.**

Where memory ran out on a path that cannot report it, the item keeps what it
held rather than allocate. Each such path is counted: `RANGES_KEPT`,
`SPANS_KEPT`, `LOST_TO_UNMAPS`, `ZOMBIES_LOST`, `MISSING_SLOTS`, `ABANDONED`,
`UNRECORDED`.

*Cost.* The system call path is unchanged within the host's noise. Under KVM,
busybox `dd` copies a million bytes one at a time, nine times a boot, with
boots alternating between the base and the branch, 54 runs each. The base
took 1.675 s at minimum and 1.723 s at the median; the branch took 1.684 s
and 1.716 s. The item grew by 2,711 lines of product code.

### F-24 — no worst-case execution time analysis
**Moderate, scoped 2026-09-25** in
[MEMORY-AND-TIMING.md](MEMORY-AND-TIMING.md) §2. Not closed, and will not be:
no WCET is claimed.

What the analysis adds is consequences. It lists what the item *does* promise
instead — EDF admission control, partitioned scheduling, bounded RT critical
sections, a preemptible kernel, interrupts that cannot steal unaccounted time —
and what each standard therefore does and does not get. It also notes that DAL
C does not require WCET as such, so this is not what blocks that rating; the
absence of any stated timing requirement to verify is, and that is F-15.

The boundary helps here more than anywhere: a WCET argument over 93,646 lines
including btrfs and a TCP stack is not a project; over the 38,989-line `core`
ring, with no recursion anywhere, it is at least conceivable.

### F-25 — no complexity, unit-size or recursion limits
**Closed 2026-09-25** by `tools/common/check/check-complexity.py`, a ratchet over
`tools/common/data/complexity-baseline.json` in the shape the item-boundary gate uses:
47 of the item's 1,887 functions sit above a floor, and the gate fails when one
gets worse, when a new one appears, or when a stale entry is left behind.

The measurement that matters: **no function in the certified item is directly
recursive.** For a kernel with no guard page under its stack that is worth
having as an enforced property rather than a belief.

Getting there needed three corrections, each a real defect in the measurement
rather than in the tree. Matching a bare name called 124 architecture-facade
shims recursive, because `fn flush_tlb` forwarding to `aarch64::flush_tlb`
names itself. `drop(x)` inside a `Drop::drop` body is `core::mem::drop`. And
taking the next `{` after a signature gave every `extern "C"` declaration the
*following* item's body, which is how the assembly symbol `ferrix_switch` came
out recursive with borrowed complexity and length scores.

A fourth was found on 2026-09-26, and it was the largest. The pattern that
stripped string literals could not cross a `\`-newline continuation, so after
the first one in a file it paired every quote with the wrong partner and read
code as string from there on. **It measured 1,559 functions of 1,887: 328
of the item's functions, 17%, were not measured at all** -- 73 of `sched/mod.rs`'s 89
functions and 71 of `main.rs`'s 76 among them. `main.rs::say_booted` scored
102 lines because it had swallowed everything up to the next string that
happened to pair; it has seven. The gate now reads source through
`tools/common/check/rustlex.py`, a lexer shared with the item-boundary gate that knows
nested comments, raw, byte and C strings, continuations, and a char literal
from a lifetime, and every run starts with its self-test. Re-measured, the
baseline went from 34 entries to 47: twelve functions that were always over a
floor and were never seen, two that were scored just under one, three whose
scores were understated (`devmgr.rs::start` by six lines), and `say_booted`
gone. No function in the item became more complex; the
measurement became less wrong, which is the one reason the baseline may rise.
The no-recursion result holds over all 1,887.

Complexity is an approximation — branch tokens, not a control-flow graph — and
the script's docstring says so, along with the two kinds of recursion it cannot
see: mutual, and through a function pointer or trait object.

### F-26 — `unsafe` is documented but not traced
**Closed 2026-09-27** by W-16.

*Was:* **Moderate.** 662 blocks, every one with a `SAFETY:` comment, one
operation each, counted per crate by `check-unsafe-audit.py`. Best-in-class as
hygiene. For an assurance argument each block in the item also needs to trace
to the requirement or hazard that justifies it.

*Now:* each of the item's 663 unsafe sites -- 523 blocks, 20 `unsafe impl`s
and 120 `unsafe fn`s, in the `core` and `item` rings, self-tests included --
opens its `SAFETY:` comment or its `# Safety` section with an obligation id,
`// SAFETY: (TRANSLATE) ...`. The ids are a closed set of fourteen, derived by
sorting what the sites do, registered in `tools/common/data/safety-requirements.json`
with the ASR, FM or AoU each serves and the code that argues it, and tabled in
[SAFETY-MANUAL.md](SAFETY-MANUAL.md) §2. Measured on the closing tree: 123
`CONTEXT`, 118 `SYSREG`, 87 `SHARED`, 66 `ENTRY`, 60 `TRANSLATE`, 55
`DEVICE`, 29 `FIRMWARE`, 29 `PROTECT`, 28 `KMEM`, 24 `FRAME`, 22 `PROBE`, 11
`DMA`, 7 `BOOT-DATA`, 4 `USER-COPY`.

*What says so in the build:* `check-unsafe-audit.py` refuses an id the register
does not define anywhere in the tree, and holds the item's untagged sites to
`tools/common/data/unsafe-trace-baseline.json`, which is empty -- so a new unsafe
site in the item without an id fails `cargo xtask check`.
`check-safety-requirements.py` holds the manual's table and the register to
each other and resolves every obligation's evidence.

*What it does not do:* an id says which requirement a site's soundness serves,
not that the site is sound; the prose after it is still the argument and still
read by a person. And the classification is itself a judgement a reviewer can
disagree with -- the rule it follows (a call is filed under what *it* does, not
under the primitive it calls) is IMPLEMENTATION.md W-16's.

---

### F-44 — the item's virtqueue barrier was empty
**Found and closed 2026-09-27** (found by ferrix-55b while fixing F-45, and
raised by the certification review).

*Was:* **Minor.** `QueueMemory::barrier` must order every access before it
against every one after it as the device sees them, and says an empty one
leaves the ring subtly wrong. The item's own, `Rings` in `pci/virtio.rs`
(the rings of stage 10's translated-domain check), was empty, on the
argument that each access is volatile. That stops the compiler; an Arm core
still reorders, so a device could read `avail.idx` before the descriptor it
published, or the queue read a used entry before the index that counts it.
The doorbell after a publish had the same gap: the index is a store to
memory and the doorbell a store to a register, which Arm does not keep in
order without a barrier either.

*Now:* `arch::dma_barrier` is `dmb osh` on AArch64 and ARMv7-A -- the
outer-shareable domain, where a DMA master sits, both directions, Linux's
`dma_wmb` and `dma_rmb` together -- and a compiler fence on x86-64, whose
stores stay in order with stores and loads with loads and whose DMA snoops
the caches, as Linux's `dma_wmb` and `dma_rmb` are there. That fence does
not order an earlier store before a later load, which TSO lets pass, so on
x86-64 the trait's "every access before it against every one after it" is
met only for what the item's queue does: it rings the doorbell after every
publish and never reads `used.flags` or `avail_event` after one.
Notification suppression or `VIRTIO_F_EVENT_IDX` in the item would need a
full fence there, as Linux's `virtio_mb` is. `Rings::barrier`
is that, and the check rings every doorbell through `ring_doorbell`, which
puts one before the register write.

*The ring-3 drivers' barrier,* `fence(SeqCst)`, is `dmb ish` on Arm. That
is enough toward a virtio device a hypervisor emulates, which is another
processor's thread in the inner-shareable domain -- Linux's virtio uses the
same "weak barriers" unless a device offers `VIRTIO_F_ORDER_PLATFORM` --
and every device those drivers serve today is one. A virtio device in
hardware, or one offering that feature, would need `dmb osh` through the
native runtime; `docs/BACKLOG.md` keeps the row.

*Checked by the build, as far as a build can:* no emulator reorders the way
an Arm core does, so no boot gate can fail without the barrier, and this
closes on the argument. What the build holds: `Rings::barrier` calls
`arch::dma_barrier`; `the_driver_publishes_descriptors_before_the_available_index`
in `src/lib/drivers/virtio` holds that the queue calls its barrier between the
descriptors and the index; and the release kernels put a `dmb osh` there
(the commit that closed it shows where). A run on real Arm is left for when
the hardware may be used, and only the Pixel 7's crosvm can give it: the DK1
has no PCI and no virtio device, so the stage 10 check cannot run there.
That run shows AArch64 only; ARMv7-A's `dmb osh` stays on the argument and
the disassembly.

### F-50 — two cores changing neighbouring GICv2 lines could lose one's setting
**Found and closed 2026-09-27** (found by ferrix-b5 reading arch/arm_common
for W-8's ARMv7-A slice).

*Was:* **Major.** A GICv2 distributor keeps four lines' priorities, or four
lines' targets, in one register, and sixteen lines' trigger configuration in
another. `gicv2::enable` changed a line's priority and, for a shared line
with none, its target by reading the word, changing its own byte and writing
the word back, and `set_edge_triggered` did the same with its configuration
bit, with nothing held. `enable` is reached from a system call on any core
(a driver unmasking its `Interrupt`) and `msi_allocate` from any core, so two
cores changing neighbouring lines at once could each write back what the
other had just changed. A lost target byte leaves a shared interrupt
delivered to no core, silently; a lost edge bit leaves an MSI on a
level-sensitive line, where the frame's pulse is never latched and the
interrupt never arrives. That is a driver's device going quiet for good, an
availability failure under O.FAILSAFE. Linux holds `gic_lock` across the
same three changes. AArch64 on a GICv2, QEMU's default `virt`, shares the
driver and had the same defect.

*Now:* every change to part of a distributor word goes through
`gicv2::rmw`, which holds `DISTRIBUTOR_RMW` (interrupt-masking, a leaf)
across its read and its write. Its lock order is written where it is
taken: a device's `minted` lock, then this one. `disable` is a
clear-enable store and takes nothing, so masking from interrupt context
under `irq`'s `BOUND` is unchanged.

*Checked by the build:* `arm_common::gicv2::check::concurrent_enables`, at
stage 10 after the device nodes are published (the `gic` line), picks the
highest sixteen SPIs no device node names, no handler is registered on, the
`GICv2m` frame does not hand out and nothing has enabled. Two tasks pinned to
cores 0 and 1 then make neighbouring lines edge-triggered and enable them at
the same moment, 64 times, held between each read and write by a widening
hook (`WIDEN_SPINS`, zero outside the check) so that the race happens every
round. Both lines must end with the default priority, a target and the edge
bit, and every round is put back as it was and read back. With the lock
removed it fails 64 rounds of 64 on ARMv7-A at `--smp 2` and `--smp 4` and on
AArch64; with it, 0 of 64 on all three. A GICv3 boot, whose distributor is
another driver's, and x86-64 say the check did not run. A confirmation on
the DK1, a two-core GICv2, is a daytime row in `docs/BACKLOG.md`.

### F-48 — an ARMv7-A handler without SA_RESTORER could not return
**Found and closed 2026-09-27** (found by ferrix-b5 reading arch/armv7a for
W-8's ARMv7-A slice).

*Was:* **Minor.** A signal handler installed without `SA_RESTORER` returns
wherever the kernel left its link register. Linux's ARM kernel points it at
its `sigpage`, a page of return sequences mapped into every process.
Ferrix's ARMv7-A had no such page and pointed it at the frame's own copy of
the sequence, on the user stack. The stack is mapped read-write and never
executable (every user page not asked to run gets `XN`), so a handler that
returned took a permission fault on its first instruction back and the
program was killed with `SIGSEGV`. musl and glibc install every handler with
a restorer, which is why no program the images carry met it; a program that
calls `rt_sigaction` itself, as Linux's ABI allows, did. The one check of
this frame had its handler exit, so the return was never taken.

*Now:* ARMv7-A has a signal return page (`syscall::sigpage`): Linux's four
sequences, `sigreturn` and `rt_sigreturn` in ARM and in Thumb, laid out as
`sigreturn_codes.S` lays them out, in one page mapped read-and-run, never
writable and shared into every process at exec, where the vDSO goes on the
other architectures. It has no data page and nothing in the auxiliary vector
names it, as Linux's does not: the vDSO image is a 64-bit ELF a 32-bit C
library could not read, so ARMv7-A still has none. A handler without a
restorer returns to the sequence for its frame in its own instruction set,
Thumb ones with the Thumb bit set, as Linux's `setup_return` computes it. A
space the page cannot be mapped into goes on without it, as a space the vDSO
cannot be mapped into does. The frame's own copy is still written, as Linux
writes it, for an unwinder.

*Checked by the build:* `arch::armv7a::trap::check::run` runs `/returning`,
whose handlers -- ARM and Thumb, plain and `SA_SIGINFO`, none with a
restorer -- each zero `r4` and `d8`, which only the return call puts back
from the frame, and return; the program requires both markers after every
`kill` and exits 55. With the old return address it ends with `SIGSEGV`
(status 139); with the page, 55. The same check maps the page into a fresh
space and reads back one read-and-run, unwritable, shared page, and the
allocation sweep maps and unmaps such a page on every architecture.

### F-45 — a virtqueue's shared indices were read and written a byte at a time
**Found and closed 2026-09-27** (found by the stage 10 seam investigation,
ferrix-55b, reading `ferrix_virtio::QueueMemory` for a "virtio-blk missing
headers" sighting, which it does not explain).

*Was:* **Minor.** `QueueMemory` provided `read_u16` and `write_u16` in terms
of its byte accessors, and every implementation in the tree took them: the
item's own, `Rings` in `pci/virtio.rs`, and the six ring-3 drivers'. So
`avail.idx` was published as two stores and `used.idx` read as two loads
while the device wrote and read them. `avail.idx` stepping from `0x01FF` to
`0x0200` was `0x0100` between the two stores, and a device that read it then
was told the ring had run backwards and stopped; a `used.idx` read across the
device's store came back with a byte from each side, which the queue refused
as a jump. Neither shows under an emulator that runs one store at a time.

*Now:* the two accessors are required of every implementation, with the
contract that each is one access of a naturally aligned `u16`, and every
implementation makes it one: `Rings` with a volatile 16-bit load and store.
The layout puts every shared `u16` at an even offset in memory that starts
16-byte aligned, so the access is single-copy atomic on every architecture.
An implementation that gives only the byte accessors no longer compiles.

*Checked by the build:* a `compile_fail` doctest on `QueueMemory` (a
byte-only implementation must not compile);
`the_shared_fields_are_touched_whole` in `src/lib/drivers/virtio`, which records
every access over 600 requests, both indices wrapping, and fails on a byte
access to any of the six shared fields; and `every_shared_field_is_even`, over
every queue size. The item's accessors compile to one `movzwl` or `movw` on
x86-64 and one `ldrh` or `strh` on AArch64 and ARMv7-A (the commit that
closed it lists them). Five of the six drivers run under `test-restart --boot
all`; the sixth, `vport`, the clipboard agent's, is exercised by no boot gate
and was checked by building it.

### F-53 — init's read-only remount at shutdown was refused, and the docs said it happened
**Found and closed 2026-09-28** (found by the certification consultant's
review of `docs/NAMESPACES.md`, reading `docs/INIT.md` §8.2 against
`src/kernel/src/syscall/fsctl.rs`; closed by that design's landing N1).

*Was:* **Minor** (the consultant's "low"). `docs/INIT.md` §8.2, step 2, says
init "calls `sync`, remounts `/` and `/data` read-only" before `reboot(2)`.
The kernel answered every `MS_REMOUNT` with `EINVAL` (`fsctl.rs`'s
`REFUSED_MOUNT_FLAGS`), so the remount never happened; init said so on the
console and went on. What a clean shutdown should have left committed and
read-only could be left for btrfs's recovery instead -- `reboot(2)` commits
both volumes itself (K7), which is why nothing was lost in practice. The
defect is outside the item: `fsctl.rs` and `fs/**` are the load ring, init
is userland, and no Security Target claim rests on a read-only remount.

*Now:* `MS_REMOUNT`, and `MS_REMOUNT | MS_BIND`, give the mount whose root
the target is the flags asked for; a mount going read-only has its
filesystem written out first, as `umount2` writes it, and afterwards every
change through it is `EROFS` (`docs/NAMESPACES.md` §2.3). Init writes a
probe file after each remount and says `/ is read-only` only when the write
was refused with `EROFS`. Since N2 (2026-09-30) a plain `MS_REMOUNT`, the
one init makes, sets the filesystem's read-only state, which every bind of
it shares, so a bind of `/` or `/data` is read-only too; the `binds` boot
line (FX-0886) checks that scope against `MS_REMOUNT | MS_BIND`'s.

*Checked by the build:* `cargo xtask test-init` requires `init     / is
read-only` and `init     /data is read-only` between `going down:
poweroff.target` and the power-off line, and `btrfs check` of the `/data`
volume after it, as before, finds it clean. Negative control, run once on
x86_64 and not committed: `MS_REMOUNT` answered `EINVAL` again (with the
boot's own mount check held back, which otherwise stops the boot first):
init said `remounting / read-only: Invalid argument` and the same for
`/data`, 2 refusals and 0 of the 2 required lines, and `test-init` failed on
F-53's line. The boot's `mounts` line (FX-0885) checks the remount itself
on every boot; its negative control, a read-only mount not refusing, failed
on its first line ("a file on a read-only mount opened for writing").

### F-54 — a channel read reported its peer closed with the peer's last message still queued
**Found and closed 2026-09-29** (found by FX-1151's investigation of the
net ring's self-check failing under WHPX; closed by b3bf8fa2; recorded by
the certification consultant's review of that commit after it landed,
2026-09-30).

*Was:* **Minor.** `Endpoint::read` in `object/channel.rs`, the core ring,
popped its inbox under the lock, let the lock go, and only when the queue
was empty asked whether the peer had closed. A peer that wrote its last
message and closed between the two made the read answer `PeerClosed` with
that message queued, and a reader taking `PeerClosed` as the end never saw
it. The net ring's check lost the kernel's `REFUSED` that way and stopped
the boot, in 2 of 3 WHPX boots on 2026-09-29. Nothing was disclosed or
delivered to the wrong reader: the message stayed in the reader's own
queue and was freed with its end. No Security Target claim rests on
delivery at a close, which is why it is Minor; a driver protocol whose
last reply is a refusal is where it would show.

*Now:* the peer's flag is loaded before the queue is popped. A peer's
writes land in the queue before its close stores the flag (`take_unread`,
a Release store under the same inbox lock the writes take), and the read's
load is Acquire, so a peer seen closed has nothing left to land and an
empty queue after the look is the end. A close after the look reads as
`Empty` with `PEER_CLOSED` already raised, and the caller's wait for
`READABLE | PEER_CLOSED` returns at once to read again. The argument needs
no write through an end after it is marked closed, and there is none:
`take_unread`'s two callers, `Endpoint`'s `Drop` and `dispose` after
`Arc::into_inner`, hold the only reference.

*Checked by the build:* the net ring's boot line (`check_net_ring`,
`stages_check.rs`) passed 5 of 5 WHPX boots after the fix, and `test-boot`
on x86_64, aarch64, armv7a and armv7a `--smp 2`. That is evidence for the
race only by repetition: it needs a preemption between two statements,
which no check forces without a hook in core code, and like F-44 it closes
on the ordering argument above. The rule the argument rests on is checked
on every boot since 7c9df826 (2026-09-30): stage 9's
`check_messages_before_the_close_are_read_first` (`object/check.rs`,
L.object.19) writes three messages, drops the writer, and requires all
three back in order and `PeerClosed` only on the fourth read, printing
`lastmsg`. It runs first of the channel checks, so a read that stops
answering from its queue fails there. Negative control, run on x86_64 and
not committed: `read` answering `PeerClosed` whenever its peer has closed,
ignoring the queue, stopped the boot on the first read with "a read told
the peer closed with its messages still queued" (FX-0901). The code before
b3bf8fa2 passes this check; it holds the rule, not the interleaving.
L.object.19 reads "not reached" until the next `cargo xtask coverage`,
which must show it reached on all three architectures.

### F-55 — a system call's copy from mapped device memory went through the direct map
**Found 2026-09-30, closed 2026-10-01** (found by os-fd running Chrome with
ANGLE on Vulkan and Mesa's software presentation on the `--venus` desktop,
where the guest stopped as `writev` copied from mapped Venus memory;
recorded by the certification consultant the same day, before the fix;
closed by c5c92781).

*Was:* **Moderate.** `AddressSpace::with_page` and `with_page_if_present`
(`user/space.rs`, the core ring) turned the physical address a program's
own page table held into `mm::direct_map(physical)` for the copy, without
asking what backed the region, and `mm::direct_map` is offset arithmetic
with no bound. A buffer in a mapping of device memory (`Backing::Device`)
therefore had the kernel read or write a direct-map address that was not
RAM. Past the direct map's span that was a kernel page fault, the safe state
reached by a program's own call: physical 0xC002BA0000 in the Venus window,
past an 18 GiB direct map, "page fault at 0xffffbb6502ba0000 (kernel read,
not mapped)" in `memcpy` under `sys_writev_at_width`. Inside the span it
would have been an access to device memory through a cacheable alias; the
loaders turn out to map only the memory map's non-MMIO regions, so a hole
there was unmapped and would have faulted too. A process that could map
device memory reached it: root through the render node, and a ring-3 driver
through its own BARs.

*Now:* both user-copy paths refuse a region whose backing is device memory
with `EFAULT` (`user::space::copyable`), right after the permission check
and before any translation, fault or alias is formed. They form a direct-map
address only through `mm::direct_map_ram`, which answers only for the RAM
runs recorded at `mm::init` from the loaders' own walk of the memory map,
the framebuffer left out, so a mislabelled backing still cannot produce a bad
alias. The panic path's backtrace stops at a frame that is not RAM. More
than 128 RAM runs fails bring-up rather than guessing (Pixel 7 and the DK1
not booted with it). Requirements L.user.107 and L.mm.62. A program that
writes from or reads into a mapped device window now gets `EFAULT` where
Linux would copy through a mapping with the window's own attributes; that
copy, for windows mapped as normal memory, is a later landing with the GPU
work, and is functionality, not this defect.

**As far as F-55 is concerned, the render node may be opened to users from
2026-10-01.** Anything that widens its permissions still comes to the
consultant, for what else a user's GPU access reaches.

*Checked by the build:* stage 9's device-copy check copies 8 times to and
from 3 device windows -- a device's aperture, a cached window past all RAM,
and one in a hole between two runs of RAM -- and requires `EFAULT` each time
before any page is faulted in, on x86_64, aarch64 and armv7a at `--smp 2`
("copies 8 copies to and from 3 device windows EFAULT ..."). Stage 2's check
requires the recorded runs to be whole pages, ascending and apart, an
allocator frame to get a checked alias, and the page past RAM and a hole page
to get none. Negative controls, run on x86_64 and not committed: `copyable`
answering true stopped the boot with "a copy through a device's page faulted
the page in before refusing it" (FX-0901); `direct_map_ram` answering for
everything stopped it with "a page past all RAM has a checked direct-map
alias" (FX-0203); and `space.rs` unfixed reproduced F-55's own fault through
`copy_from_user_through` from `sys_writev_at_width`.

### F-58 — a VT-d unit whose walk does not snoop could read stale table entries
**Found 2026-10-02, closed 2026-10-02** (found by the certification
consultant at the review of the NVIDIA N0g design, `nvidia-n0-designs`
adb49fdfe, ruling 1 and condition G8; pre-existing, not introduced by any
NVIDIA branch; closed by branch `f58-coherency`).

*Was:* **Moderate.** `vtd.rs` never read `ECAP.C`, the unit's page-walk
coherency, and nothing in the kernel issued `clflush`. Root and context
entries (`write_entry`), fresh tables (`table()`'s `zero_frame`) and
second-level entries and tables (`mm::map_io`, `mm::unmap_io`) were all
written through the write-back direct map. A unit with `C` clear reads its
tables from memory, past the processors' caches, so on one a cleared entry
could still be read as present after its invalidation -- a device reaching
a frame already given back, against H.DMA.2 and H.DMA.3 -- and a fresh
table over stale memory could be read as present entries. QEMU's unit
reports `C` clear but walks coherently, so no gate could show it. The
same class was confirmed in `smmuv3.rs`: it never checked `IDR0.COHACC`,
never wrote `CR1`, and gave each stream's stage-2 walk non-cacheable,
non-shareable attributes (`S2IR0`, `S2OR0` and `S2SH0` zero), while its
stream table, queues and tables are written through the cached direct
map.

*Now:* a VT-d unit with `C` clear has every write to a table it walks
noted in a record (`ferrix_paging::coherence::Unpublished`), and cleaned
to memory -- `clflush` of each line, then `mfence`
(`arch::clean_for_walker`, its line size read from `CPUID` once) -- before
the change that wrote it is published: before the invalidation that
publishes an attach or a detach, and before a map or an unmap returns,
since outside caching mode no invalidation follows a map. The mapper's own
writes go through the record (`ferrix_paging::coherence::Walked`), and a
fresh table frame is cleaned whole before anything links it (`vtd::table`,
which N0g's interrupt remapping table and invalidation queue use too).
Each change checks its record is empty at its own publish point and is
refused if it is not, failing closed: an attach or a detach before its
invalidation, a map with `MapError::NotCleaned`, so the pin unwinds, the
page it wrote included, and an unmap with the same error, so the frames
are kept as for an invalidation that never finished. That every write to
memory the unit walks is noted rests on construction, not on the check:
`write_entry`, `table` and `Walked` are `vtd.rs`'s only writers of that
memory. A unit with `C` set is written as before, with nothing noted or
cleaned. The register-based invalidations are unchanged; N0g moves the
publish point to its queue. An `SMMUv3` without `COHACC` is now left
alone, with the reason printed, rather than cleaned for: no machine the
kernel runs on has one. One that is brought up gets `CR1` set to
write-back cacheable, inner shareable accesses to its stream table and
queues, read back, and a write-back, inner shareable stage-2 walk in each
stream table entry, as Linux's `arm_smmu_device_reset` and its stage-2
entries have them. Requirements L.iommu.56, L.iommu.57 and L.iommu.58.

**No unit that does not snoop was available.** QEMU's VT-d reports `C`
clear but walks coherently, and its `SMMUv3` reports `COHACC`. So the
closure's evidence is QEMU running the `C`=0 path on every x86-64 boot,
the checks below that each change's writes were cleaned before it was
published, and the guarantees of the Intel SDM (`CLFLUSH` and `MFENCE`)
and of the `SMMUv3` specification (`COHACC`, `CR1`, the STE's walk
attributes). It is not a demonstration on hardware of a stale read
prevented.

*Checked by the build:* stage 10's IOMMU check prints what the VT-d unit
noted and cleaned, and how many changes found their writes cleaned before
their own publish point -- "26 entry writes and 10 fresh tables noted and
cleaned to memory on 1 VT-d units that do not snoop; 11 changes each found
them cleaned before its own publish point" on x86-64, the counts following
the tables the boot's pins add -- and fails the boot (FX-1003) on a change
refused for a write not cleaned, or on a unit that does not snoop and
cleaned nothing; test-boot requires the line on x86-64. The check proves
that each change's noted writes were cleaned; that every write is noted is
the construction above. Host tests in `ferrix-paging` show every entry the
mapper writes, on a map and an unmap, cleaned after it is written, each
table it adds cleaned whole before its link is written, and nothing
cleaned for a walker that snoops. On AArch64 the `SMMUv3` comes up with
`COHACC` checked and `CR1` read back, and faults the out-of-domain write.
Negative controls, run through the fleet's `gate.sh control` and not
committed: the clean before an unmap's return dropped had the unmap
refused ("a VT-d change was refused: a table write was not cleaned to
memory before it was published"), the frames kept, and the x86-64 boot
stopped at stage 10's next pin; the clean before an attach's invalidation
dropped had both attaches refused ("gets no translated domain: a table
write was not cleaned to memory before it was published") and stopped
the boot naming the VT-d changes refused for a table write not cleaned to
memory; and `COHACC`'s test inverted left AArch64's
unit alone ("its table and queue accesses do not snoop the caches"), and
test-boot failed on no write outside a translated domain faulted.

### F-59 — a pin closed on an untranslated domain was kept for good
**Found 2026-10-02, closed 2026-10-02** (found by the certification
consultant at the review of NVIDIA N0f's fix-forward, `nvidia-n0f-fix`
355e96190, from the author's report that its premise was false;
pre-existing, made bounded by N0f; closed by branch
`f59-untranslated-pins`).

*Was:* **Moderate.** On an untranslated domain -- every device on ARMv7-A,
the DK1 board's display and sound among them, and any device behind a unit
the kernel refused -- `Pin::drop` gave a closed pin's frames back only on a
translated domain, so every pin a driver closed there, live or dead, was
kept for good. Drivers pin per request as well as at start: `virtio-gpu`
and `stm32-ltdc` pin each buffer attached to a scanout, and `virtio-snd`
each published stream buffer. Before N0f that leaked kernel memory without
bound (T.EXHAUST); after it the pages counted as `kept` against twice the
device's budget, and the device stopped pinning after some 170 attaches of
a 1024x768 framebuffer at the default (availability).

*Now:* a pin a live driver closes on an untranslated domain is given back
at once, as on a translated one, and leaves `live`. Such a domain is the
degraded trusted mode of VULNERABILITY-ANALYSIS V-03: its device can reach
all of memory, so keeping the frames protected nothing it could not reach
anyway. A dead driver's pins there go to the quarantine like a translated
domain's -- the record is made with every pin now -- counted as
quarantined, with nothing to unmap, and released at the next HELLO the
device's core accepts (`DeviceNode::hello_accepted`, after its
configuration is read back). `kept` grows only from an unpin a domain
refuses and the could-not-quarantine fallback. Requirements L.object.126
and L.object.127; L.object.46 and L.object.47 reworded; SAFETY-MANUAL
AoU-12 and T.EXHAUST path 7 state the bound again.

*Checked by the build:* on ARMv7-A, stage 10 opens and closes six 2-page
pins on an untranslated domain at a budget of 2 pages -- three times past
twice it -- requires each taken, the counts back at nothing and each frame
left with only the check's own reference (P7); and closes a 2-page pin of a
dead process, requires it quarantined with its frames held, calls the
node's `hello_accepted`, and requires the counts at nothing and the frames
freed (P8). The line is "untranslated pins: 6 a live driver closed given
back at once, past twice a budget of 2 pages; 2 pages a dead driver left
quarantined and freed at the next HELLO". Negative controls through the
fleet's `gate.sh control`: the live close left as `translated()` stopped
ARMv7-A's boot with "a pin closed by a live driver on an untranslated domain
was kept", and the quarantine record made only on a translated domain
stopped it with "a dead driver's pin on an untranslated domain was not
quarantined".

### F-60 — a leave from a speculation domain can miss a processor installing the member's space
**Found 2026-10-02, closed 2026-10-03** (found by the certification
consultant at the review of `docs/OPAQUE-KERNEL.md` §9.8 on
`ipc-step23-design` e809d98f8, from the author's question 5; pre-existing
since the speculation domain landed, bf9efba95, not introduced by §9.8;
closed by branch `f60-domain-leave`).

*Was:* **Moderate.** A member that leaves its domain (§9.3a A1, §9.3b F1) must have every
processor whose last space was in the domain issue the predictor barrier
before the leave returns. `Process::leave_speculation_domain`
(`object/process.rs`) stores `OUT` as its space's domain
(`AddressSpace::leave_domain`, a `Release` store), then
`arch::leaving_domain` (`arch/speculation.rs`) loads each processor's
`LAST_DOMAIN` and asks for the barrier where it equals the domain, then
`smp::synchronize` waits for every processor's grace-period answer, which
serves the request.

The installing side is the mirror image: a processor switching to the
member's space reads the space's domain (`AddressSpace::install`, then
`entering_space`) and later records it in its `LAST_DOMAIN`
(`entered_space`). The leaver's store and its later loads are a
store-buffer pattern against that read and that record. Nothing orders the
`Release` store of `OUT` before the loads of `LAST_DOMAIN`, so the leaver
can read a processor's old `LAST_DOMAIN` while that processor has already
read the domain as it was, and records it a moment later. That processor
then runs the member's space, inside the domain's predictor state, with no
barrier wanted, and the grace period's answer finds nothing to serve.

*Where it is open:* x86-64, whose store buffer lets the later loads pass
the store; ARMv7-A, whose `DMB` placement for a `Release` store does not
order it before later loads; and the Rust memory model, under which the
outcome is allowed whatever the hardware does. AArch64 closes it by its
instructions: a `STLR` is ordered before a later `LDAR`.

Two smaller gaps went with it. The interrupt handler served the scan's
request before it read the generation it answered, so a request made in
between waited for the next interrupt, after the leave had returned. And a
processor waiting in a grace period of its own answered the leave's
without serving the request at all.

*Now:* the local check the consultant accepted, under its conditions (a)
to (d). `leaving_domain` publishes the domain in a free slot of `LEAVING`,
a set of eight, by a `SeqCst` compare-exchange. It does so before the
scan, and so before the grace period's `SeqCst` increment and its
interrupt (b). Two leaves of different domains at once each keep their
own slot, and a ninth yields until one is free (a). The leave now waits
for the grace period inside `leaving_domain` and gives the slot back
after it. Every processor answers a grace period through one function,
`smp::answer_grace_periods`, from the interrupt handler or while waiting,
with interrupts masked and so after any switch in progress. It reads the
generation first, then serves the scan's request, then compares its own
`LAST_DOMAIN` with every slot (`arch::answer_leaving`), and on a match
issues the barrier and clears it (b). An answer that counts for the
leave's grace period read a generation the publish came before, so it
sees the domain. Once it has seen the domain, it reads the space's domain
as `OUT`, so no later install of the leaver's space skips the barrier.
The same order closes the two smaller gaps. L.object.116 names the
mechanism (d). SPECULATION.md §3 and MEMORY-AND-TIMING §2.2a describe the
leave, and `docs/OPAQUE-KERNEL.md` §9.3c records the conditions. One
behaviour changed beside the fix: a kernel built with `--mitigations off`
no longer waits for a grace period when a member leaves, since it has no
barrier to wait for.

*Checked by the build:* the `domain` line's case 11, at two processors or
more on every architecture (c). It runs a kernel task on another processor
and makes that processor's last domain none. It then arms a hook that
`leaving_domain` runs between its scan and its grace period
(`arch::CheckHook`, which records the check that armed it). At the hook
the task installs and leaves a member's space, so its processor records
the domain the scan has just passed. The case requires the hook to have
seen that, and the processor to have decided the barrier before the leave
returned. The line ends "and where its domain was recorded after the
leave's scan". Only stage 9 arms the hook, the case disarms it, and a boot
check before the success marker stops the machine with FX-0908, naming
the check, if a hook is still armed. Negative controls, run by hand in the
branch's worktree and not committed (logs in
`~/.local/share/ferrix/logs/f60/`): `answer_leaving` made a no-op stopped
the x86-64 boot with "case 11: a processor that recorded the domain after
the leave's scan issued no barrier before the leave returned"; and the
case's disarm removed stopped it with FX-0908 ("a check's hook was still
armed after stage 9"). The interleaving the hook forces is one the
store-buffer pattern allows. No emulator can be relied on to produce that
pattern by itself, so the case shows that the local check catches a late
record, not that the pattern occurs.

### F-62 — coverage evidence left uncarried, so traceability read wrong lines
**Found 2026-10-05, closed 2026-10-05** (found by po5-c1 working the
certification row C1, "debt after landings", and recorded by its
certification consultant, ledger line 359; F-61 is reserved for land-n6's
review).

*Was:* **Minor**, a process finding. Each `coverage-<arch>.json`'s
`verification` map names the lines a coverage run saw each check reach,
and `TRACEABILITY.md` reads a requirement as reached on an architecture
when its verifier's lines are there. Most of those lines are in load-ring
check files. The devtty landing (5dddc1981, 2026-10-03) put 54 lines into
`src/kernel/src/syscall/check.rs`, and console-revoke (0f94a6d1a,
2026-10-04) another 147; both touched no core or item file, and neither
carried the evidence. Netns's carry onto `main` (61135f4ba) then took its
base from a tree after both, which `carry-coverage.py` allowed, so the
stale lines were carried on as if they were right. Every page's check
still passed. From then until 2026-10-05, `TRACEABILITY.md` read
L.x86_64.68 (since 2026-10-03), L.user.62, L.syscall.23 and H.MEM.16 as not
reached on all three architectures, though their checks are reached, and
L.syscall.21 as reached on all three, though no coverage run has reached
its check, which came in after the last measurement.

*Now:* the evidence is carried again from 0498203ca, the last tree whose
`syscall/check.rs` anchors were right (02f8b6d23), and the five rows read as
measured. `carry-coverage.py` carries each of the nine evidence files from
the commit that last wrote that file, by default, and refuses a `--from`
whose kernel is not the one each file was last written on.
`carry-coverage.py --check` carries each file in memory from its own
commit to the tree and fails if any would change. That is, the kernel moved
a line the evidence names and nobody carried it. Each file has its own base
so that a later edit of one file, an argued one say, cannot move another's
base past a kernel change nobody carried (the consultant's D1, ledger 362).
One limit stays: an edit of a `coverage-<arch>.json` by hand resets that
file's own base, and the gate then judges it from the edit.
`gen-coverage-justification.py --check` runs it, so every `cargo xtask
check` and `check-docs` does. A shallow clone, whose oldest commit only
seems to write every file, is not judged. CONVENTIONS.md's review list and
VERIFICATION.md's *Between measurements* say that any landing moving a
line the evidence names carries it, load-ring check files included, and
that the review reads `TRACEABILITY.md`'s reached and not-reached changes.

*Checked by the build:* `--check` in every `check` and `check-docs`.
Controls, run with the new scripts copied onto older trees (logs in
`~/.local/share/ferrix/logs/po5-c1/ctl-carry-*.log` and
`ctl-gcj-check-5dddc1981.log`): on devtty's 5dddc1981 and console-revoke's
0f94a6d1a it fails on all three `coverage-<arch>.json`, "anchors not
carried since 14fafd03c201", where the old scripts passed; through
`gen-coverage-justification.py --check` on 5dddc1981 it fails the same way.
On 0498203ca, 14fafd03c and 3e8b759d3, each a tree as its carry left it,
it passes. Netns's carry replayed on its parent ea62d4e5e with `--from
def906ba2` is refused, "the kernel at def906ba2 is not the one the
evidence was last written on (14fafd03c201)", where the old script
carried. On `main` before the fix (3c08d5657) it passes: once the
laundering carry had written the evidence, the stale lines are what the
latest carry wrote, and only a coverage run or a carry from an earlier
tree, as here, can tell. The gate stops the step that loses the lines,
not lines lost before it. In particular it does not show that the
`verification` maps were carried right before 0498203ca, the tree this
repair started from: the proof of those is the next `cargo xtask coverage`
on all three architectures, the re-measure `docs/BACKLOG.md` already holds
(F-10, its 2026-09-27 row), which writes every map afresh. A further
control, the consultant's (ledger 362): on a scratch tree from `main`, one
commit moving `syscall/check.rs` by a line with nothing carried, then one
editing only `coverage-argued-x86_64.json` by hand; the per-file gate fails
on all three `coverage-<arch>.json`, where de076fe2e's single base passed
(`ctl-d1-two-commits.log`).

### F-63 — the queue to a chardev driver unbounded under a spin lock
**Found 2026-10-05, closed 2026-10-05** (found by the certification
consultant's review of NVIDIA's N10 text, `MEMORY-AND-TIMING.md` §2.2k,
ledger line 361, and fixed by po5-c1 and po6-c1 with N12 and N13).

*Was:* **Moderate**, load ring, on the images that run `nvrm`, which is
`run-nvidia`'s alone; pre-existing since N1e (2026-10-03). The chardev
core (`src/kernel/src/interfaces/chardev`) keeps a table of a control's
outstanding requests, at most 256, and a queue of what its task has yet
to write to the driver, reserved when the control is made for 256 requests
and one release per open file. A request abandoned on a signal left the
table but stayed in the queue, and the task writes the queue only while
the driver's channel has room. So with a driver that stops reading, a
program that is signalled and calls again keeps admitting requests while
the abandoned ones pile up in the queue past its room. The queue's
`push_back` in `call` was then an allocation that cannot report failure,
under the control's spin lock, and could run until the kernel stopped on
the allocation-error panic. The nodes are `0666`, so any local user could
drive it.

*Now:* the control counts the requests in its queue (`queued`), and
admission refuses `EBUSY` while the table or the queue holds 256. An
abandoned request still queued is taken out of the queue at once, and the
task skips a request answered or abandoned before its turn, so the queue
holds at most 256 requests and one release per open file, the room it was
given, and its `push_back` never allocates. A release queued while the driver
dies reads that it is gone under the control's lock, so it is never pushed
into the queue `finish` has emptied (the consultant's C1, ledger 373).
The bound and its cost are in
`MEMORY-AND-TIMING.md` §2.2k. The consultant's advisory on the same review
is met as well: `create` and `run` reserve the start and control lists'
room and push under one lock guard, so no other push takes the room
between them.

*Checked by the build:* stage 10's chardev self-check
(`interfaces/chardev/check.rs`, NVIDIA's N12, panic FX-1013) plays the
driver from a process given a PCI function and the programs from kernel
tasks. With the driver reading nothing it takes requests in until `EBUSY`
and abandons them, three rounds of 256, and requires the queue empty after
each and a 257th request refused each round; then the driver answers 256
requests before their turn, and the check requires the queue at most 256
while they wait in it, and empty once the driver reads again. On every
boot with a PCI function it prints, on all three architectures alike,
"chardev  11 HELLOs, copies and requests refused as specified, 3 requests
answered, 768 abandoned with the queue to a driver that reads nothing at
most 256 requests" (`gate.sh` tags `po6-c1-tip-boot-*`). Nine
negative controls (N13 and the fix's own two), each a one-line sabotage
that must stop the boot on its own message, all fired on the landing's
tree, x86-64 under KVM (`gate.sh` tags `po6-c1-ctl-*`, INDEX lines of
2026-10-05T15:41Z to 15:45Z): admission not counting the queue ("the queue to a driver that
reads nothing held more requests than its room"), abandonment leaving the
request queued ("abandoned requests stayed in the queue to a driver that
reads nothing"), abandonment leaving it in the table, the drain skipped on
abandonment and on the answer (L1), the HELLO's name allowlist skipped, a
257th request admitted, `DUPLICATE` left on the driver's end, and the copy
lookup ignoring its control. The same nine fired first on po5-c1's
09e020915 (tags `po5-c1-f63-c8-guard`, `po5-c1-f63-c9-abandon`,
`po5-c1-n13-c1` to `c7`).

## F. Organisational

These cannot be closed by engineering. They are recorded because an audit that
omits them is not an audit.

### F-27 — no independence
**Blocking** for formal certification at any level. No independent verifier,
validator or assessor. EN 50716 at SIL 2 is permissive — roles may be combined
with justification — but DO-178C DAL C still requires independence for 5 of its
62 objectives, and a CC evaluation requires an accredited laboratory by
definition.

### F-28 — no quality management system
**Blocking** for IEC 62304, which presumes ISO 13485. No documented
configuration management procedure, problem-resolution process (§9) or
maintenance plan (§6) in standard terms. `docs/CONVENTIONS.md` is 135 lines
about commit authorship and agent coordination.

### F-29 — development security is not demonstrable
**Moderate** now; **Blocking** at EAL6+ (`ALC_DVS.2`). Development happens in
ephemeral cloud containers with AI agents as authors. The one-author-per-commit
gate is real provenance control and is not a controlled site with personnel
vetting and need-to-know.

Also genuinely novel: no scheme has settled how to treat AI-authored code in a
certified item. It should be raised with a certification body early rather than
discovered at assessment.

### F-30 — no field history
**Moderate.** Every rating here is argued from construction and verification.
The proven-in-use credit that IEC 61508 route 2s and EN 50716's prior-use
provisions offer Linux is unavailable to a kernel this young.

---

## Closed

### F-00 — the kernel had no structural coverage measurement
**Closed 2026-09-25** by `tools/common/gen/coverage-report.py` and the
`FERRIX_QEMU_PLUGIN` hook. Superseded by F-10 to F-13, which are about the
*level* of coverage rather than its absence.

### F-0A — the certified item's SOUP was unenumerated
**Closed 2026-09-25** by `tools/common/gen/gen-soup.py`, which measured it as empty and
now fails the build if that stops being true.
