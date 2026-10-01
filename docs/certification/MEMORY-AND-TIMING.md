# Memory and timing

The two determinism arguments the safety standards ask for, against the item in
[ITEM.md](ITEM.md): what the item allocates and what happens when it cannot,
and what it promises about time.

Both analyses began by concluding that the property is **not** achieved. That
was the point of writing them: findings F-23 and F-24 were vague statements
that something was missing, and this replaced them with measured statements of
exactly what was missing and what it would cost. Since 2026-09-26 the first
half holds for the item: allocation failure is reported rather than fatal, and
a gate keeps it so (§1). What is still not claimed is a bound on memory, and
the second half, time, is unchanged.

---

## 1. Dynamic memory — F-23

### 1.1 What the item allocates

The item carries four allocators:

| Allocator | Role |
|---|---|
| Buddy (`src/lib/kernel/frame`) | physical frames |
| Kernel heap (`src/lib/kernel/heap`) | `GlobalAlloc` behind `Box`, `Vec`, `Arc`, the maps |
| `vmap` arena | kernel virtual address space for device windows and stacks |
| Demand paging / CoW (`user/vmo.rs`) | a program's pages, on first touch |

The frame allocator, the arena and demand paging always reported failure, as
`Option` or an error. The heap did too, and `GlobalAlloc` turned that into a
null pointer that every ordinary container sent to the allocation error
handler. So the question is the heap's callers. On 2026-09-25 they were
**225 sites across 40 files**, measured by hand, and every one was fatal.

They are now counted by `tools/common/check/check-fallible-alloc.py`, the "fallible
allocation" step of `cargo xtask check`. It finds every call to an allocating
standard-library API in the item's product code: the constructors, `vec!` and
`format!`, and every method that can grow a collection; a `Type::default()`
of a type whose `Default` allocates, wherever in the kernel that `Default` is
written; and a derived `Clone` over an owned heap field. It also reads three
load files as it reads the item, the ones the item's own `process_create` and
`process_start` run (§1.3). It fails on a site that is not argued. On
2026-09-26 it reads:

| | Sites |
|---|---:|
| Unmarked in the item: an infallible allocation | **0** |
| Unmarked in the load files the item runs (`syscall/process.rs`) | 14, recorded as debt |
| `NOALLOC:` — cannot allocate: room reserved just before, or a type that only looks like a collection | 36 |
| `FALLIBLE:` — a first-party method named like a standard one, that reports failure | 13 |
| `FATAL-ALLOC:` — bring-up, fatal by design (§1.3) | 73 |

Everything else in the item allocates through `src/kernel/src/fallible.rs`, and
the gate does not flag it. Its ratchet baseline,
`tools/common/data/fallible-alloc-baseline.json`, records `process.rs`'s 14 and nothing
else: a count may fall and not rise, and a new unmarked site fails the build.

### 1.2 How failure is reported

The obvious fix is unavailable. `#[alloc_error_handler]` is an unstable
library feature (rust-lang #51540), and so are `Box::try_new`, `Arc::try_new`
and `BTreeMap::try_insert`, verified against the pinned 1.97.1. `src/kernel/` and
`src/boot/common/uefi/` use no unstable features by policy, and `TOOLS.md` leans on that.
Only `Vec::try_reserve` is stable. So fallible construction is built from
stable parts, in two kinds:

* **What can be made fallible directly.** `src/lib/kernel/fallible` (`ferrix-fallible`)
  gives `Box`, `Vec`, `VecDeque` and `String` fallible constructors.
  `try_box` allocates `Layout::new::<T>()` through the global allocator and
  makes the box with `Box::from_raw`: `Box`'s documentation makes that
  conversion part of its contract. `try_boxed_slice` and `try_boxed_str`
  allocate exactly once. The collection helpers reserve with `try_reserve`
  before they grow. Room already there is not an allocation, so it is never
  failed, even under injection. The crate is host-tested against a recording
  and refusing global allocator, and run under Miri in CI.
* **What cannot.** `Arc::new`, `Arc::new_cyclic` and a map insert allocate
  inside `alloc`, with layouts it does not publish. They run in a *reserved
  section* (`src/kernel/src/mm/reserve.rs`). Entering the section masks this
  processor's interrupts, then fills its reserve to 16 objects of every heap
  size class, plus one block for an `Arc` too large for a class. It fails
  with `AllocError` if the heap cannot supply them, and that failure is the
  one the caller reports. Inside the section, an allocation the heap refuses
  is served from the reserve. So the operation either never starts, or it
  runs to the end on memory set aside for it. The depth argument: an `Arc` is
  one allocation of `arc_layout::<T>()`, and a B-tree insert is at most height
  + 2 nodes of at most `btree_node_bound`. The host tests measure both against
  the pinned standard library, and `fallible.rs` checks each map's node size
  against the largest class at compile time. A tree of height 14 has more
  than 10^11 entries. Soundness does not rest on the bound: a reserve block is
  handed out only for a request of its own class, so a wrong bound would let
  the allocation fail as it did before (a stop, FX-0008), and would corrupt
  nothing.

Each caller turns `AllocError` into the answer its interface has:
`NO_MEMORY` from a native call, `ENOMEM` from a Linux one (`mmap`, `mremap`,
`fork`, a page fault that must copy), `EAGAIN` from `madvise`, and a refused
step at bring-up. Where a change has several steps, each takes its room
before the first changes anything, or undoes what went before. Mapping an
object inserts into the table, attaches, and places the region, and takes all
three back when the last one fails. `fork` copies every table fallibly before
it marks the parent's pages copy-on-write. A child it then cannot finish is
let go, and the parent keeps the marks, which only make it copy what it
writes. `mremap` reserves room in the map for both removals and the region's
return before it takes anything out.

Three paths were rebuilt so that they need no memory at all:

* **The scheduler.** It used to allocate a tree node on every enqueue, and so
  allocated with the run queue locked and from interrupt context. Every task
  now lends the run queue and the sleepers' timeline a node of its own, made
  when the task is made. `src/lib/kernel/sched/tests/no_allocation.rs` counts the
  allocations of a queue and a timeline at work under a counting global
  allocator, and requires none.
* **Taking pages out of an object** (munmap, madvise, truncation, mremap,
  copy-on-write). The frames go into a list whose room is had before the
  first frame leaves the object, so a frame is never out of one list and not
  in the other. A decommit that cannot be refused, and so cannot fail, falls
  back to 32 pages at a time on the stack. With no memory to list the spaces
  that map the object, it asks them one at a time, in the order of a key that
  does not move, each with a shootdown of its own.
* **Closing an object.** A drop that would recurse is queued. When the queue
  cannot grow, the object is dropped in place, at most four deep, and only
  past that is it given up and counted.

Where a path cannot report failure and cannot avoid allocating, it keeps what
it held rather than allocate: an unmap that cannot note a range leaves its
pages with an object that no region shows them through. Each such path is
counted, and every counter stays zero while memory lasts: `RANGES_KEPT`,
`SPANS_KEPT`, `LOST_TO_UNMAPS`, `ZOMBIES_LOST`, `MISSING_SLOTS`, `ABANDONED`
and `UNRECORDED`.

**The negative control.** Every boot runs `object/alloc_check.rs` at stage 9
(FX-0902). With the heap made to refuse every allocation inside a section, an
`Arc`, a large `Arc` and 200 map inserts must complete on the reserve alone.
With the reserve refused its filling, they must fail before they start. Then
one process drives rounds of native calls that allocate, while every *n*th
allocation of its task fails, for six prime periods. Every call must succeed
or answer `NO_MEMORY`, and no port may lose a promised packet. A clean round
must then succeed, and no frame may have leaked. Last, a decommit of 80 pages
of a mapped object must give every page back with every allocation failing,
through both fallbacks above, and leave no translation. It reads the same on
all three architectures: *"486 native calls with 162 allocations failed under
them: 150 answered NO_MEMORY, the rest succeeded, nothing leaked; 35
allocations served from a reserve; 80 pages decommitted with none"*.

### 1.3 What stays fatal

**Bring-up**, by design. 73 sites run before the first program or while a
processor comes up, where there is nothing to return an error to. Each is
marked `FATAL-ALLOC:` and listed by the gate's `--report`:

| File | Sites | What |
|---|---:|---|
| `device.rs` | 17 | the device registry, from the firmware's tables |
| `devmgr.rs` | 12 | the device manager's start: driver list and arguments |
| `sched/mod.rs` | 11 | per-processor run queues and idle tasks |
| `iommu.rs` | 10 | translation units and their domains |
| `pci.rs` | 9 | bus enumeration |
| `smp.rs`, `arch/*/smp.rs` | 12 | per-processor data and secondary start-up |
| `init.rs`, `vmap.rs` | 2 | the first program's arguments; the arena |

A failure there stops the machine, and the panic handler names it. It knows
std's *"memory allocation of N bytes failed"* message, when the heap has
refused, and reports **FX-0007** before the boot completes.

**The load.** The uncertified load's allocations are still infallible, and it
shares the heap. One that fails after boot stops the machine with **FX-0008**.
That is an application condition, AoU-5, not a property of the item.

**The load the item runs.** That line is clean for the Linux personality,
which is load from its entry, and not clean for two native calls, which are
item calls that run load code. `process_create` makes a POSIX process through
the `Processes::load` hook (`syscall/launch.rs`), and `process_start` makes
its first thread. F-23 was scoped to the item's source, and on that scope it
holds. But a refused allocation in that load code stops the kernel on an item
path, which the scope does not excuse and the claim of "every site a program can
reach" did not allow for. Found on 2026-09-26: `Signals::default` built the
signal tables with `vec!`, under every process and thread. It is now fallible,
checked at stage 7 with a negative control, and the gate reads `signal.rs`,
`thread.rs` and `process.rs`. What those two calls still reach that cannot
report failure:

| Where | What |
|---|---|
| `syscall/process.rs` | the program name and arguments recorded (`record_exec`), the thread and task lists a start pushes onto -- among the 13 the gate records |
| `syscall/registry.rs` | the `Arc` the new process is registered in |
| `syscall/fd.rs` | `standard_streams`: the console's open description, whose `Arc` `OpenFile::new` makes. The table grows fallibly (F-37) and a refusal is `NO_MEMORY`, not a stop (2026-09-26; stage 7 checks it with every allocation failing); only a console that cannot be opened at all, a kernel bug, still stops it (`CONSOLE_DESCRIPTORS`) |
| `syscall/load.rs`, `syscall/exec.rs` | the ELF loader's lists |

Each of these is the load's and covered by AoU-5, as it was before; the
difference is that the list is now written down and the three files the item
leans on most are gated. Converting the rest is the load-side work §1.5's
third item declines, done one path at a time as the item comes to depend on
it.

**What the gate cannot see.** It says so in its docstring:

* It matches methods by name, not type, which is what `NOALLOC:` is for.
* `.clone()` is not flagged. The item's 18 were audited by hand on 2026-09-26,
  and none allocates. Each is an `Arc`, a `Weak`, an `Option` of one, an
  `Object` (an enum of `Arc`s) or a `FileMapping` (an `Arc` and a flag).
* It does not see conversions that allocate, `write!` into a `String`, or
  allocation inside a callee. `src/lib/kernel/vma`, `src/lib/kernel/objects`, `src/lib/kernel/sched` and
  `src/lib/kernel/sync` were converted with the item. The other libraries the item calls
  allocate nothing on its paths. The load's callees are the table above.

### 1.4 Against the standards

* **EN 50716 Annex A** discourages dynamic memory at SIL 2 and above. The item
  still uses it, on paths a program can drive. What changed is the failure
  mode: exhaustion is now an error the caller sees. It was a stop of the
  machine.
* **DO-178C** requires an argument that allocation cannot fail in a way that
  defeats a safety requirement: exhaustion, fragmentation and timing.
  Exhaustion is now argued: it is reported, at every site, and checked by the
  build and on every boot. Fragmentation and the time an allocation takes are
  not analysed.
* **IEC 62304 §5.5** wants each unit's failure behaviour stated. It is now
  per interface, and it is an error return, except at bring-up.

### 1.5 What is not claimed

1. **A bound.** Nothing bounds what the item allocates. Measuring the
   pre-user-mode working set would give bring-up a bound. The paths a program
   drives would still be unbounded.
2. **A quota on the whole heap.** Since 2026-09-26 a job is charged for
   the frames of its programs' memory and page tables, the native objects
   they make and their tasks, each against a limit (F-35, §1.6), and for
   the kernel heap the Linux personality holds for its programs -- open
   files, dentries, tmpfs inodes and names, pipe and socket buffers,
   messages and descriptors in flight, epoll registrations, regions, locks
   -- against the same memory limit (F-37, §1.6). What stays charged to no
   job is argued in §1.6: what one task holds, which the task limit bounds,
   and a few tables the machine shares, each with a fixed bound. A job
   without a limit still drives the heap as far as the machine's memory,
   and that is AoU-5's to configure.
3. **The load.** Converting the load ring the same way would take AoU-5's
   second half away. It is outside the item and not attempted.
4. **Preallocation.** What a SIL 4 or DAL A item would do, and incompatible
   with an OS that also hosts a compiler.

**Verdict: F-23 is closed** for what it measured: allocation failure in the
item's own source is reported, not fatal, and the build says so. Two item
calls still run load code whose allocations are fatal (§1.3), which the
closure did not claim and the first version of this section implied. The bound it also names is
not claimed, and is exported to the integrator as AoU-5.

### 1.6 Job quotas — F-35 and F-37

What a job's programs hold at once is charged to the job and limited there
(`src/kernel/src/object/quota.rs`; the design is IMPLEMENTATION.md W-13, and
W-15 for the kernel heap). A job below the tree's root has a slot of
atomics -- use, limit and refusals for memory (in bytes), objects and
tasks, and the part of memory that is kernel heap -- and a charge walks the slots from the job up,
with a compare-and-swap at each, so no use passes a limit anywhere above it,
even for an instant, and a refused charge takes nothing.

* **Memory** is charged when a frame of a program's memory is allocated
  (`mm::allocate_user_frame`: a fault's commit, a copy-on-write copy, the
  page cache's fill) and when `map_in` builds a page table for a user
  space, to the job of the task that caused it. The frame record keeps the
  slot's index in the link an allocated frame does not use
  (`ferrix_frame::Frames::set_owner`), so `release_frame` and
  `deallocate_frames` take the charge back wherever the frame is freed, with
  no lookup and no lock -- the deferred page-table frees of F-36 included. A
  refusal is an allocation failure, which §1.2 made an answer everywhere.
* **Kernel heap** the Linux personality and its libraries hold for a
  program is charged as memory too, against the same limit, as cgroup v2
  folds `kmem` into `memory.max` (F-37). A token from `src/lib/kernel/kmem` is made
  where the allocation is -- to the job of the task whose call made it, at
  the size class the heap serves it from -- and kept inside the object, so
  every free path uncharges it. A buffer that grows is charged before it
  grows, to its object's job; a message in a socket's queue to its writer;
  a descriptor in flight stays its opener's. A refusal is `ENOMEM` from the
  call, before anything changed. The kinds, and what is argued rather than
  charged, are in FINDINGS.md F-37: a kernel stack, a futex waiter and a
  process's recorded arguments are one per task, which the task limit
  bounds; 256 pseudoterminals, the neighbour and IP reassembly caches, the
  routing tables and a btrfs transaction's changed nodes are the machine's,
  each with a fixed bound; a VMO's page list is a few dozen bytes per
  charged frame. `memory.stat`'s `kernel` line reads the heap alone.
* **Objects** carry a token that uncharges as the object drops.
* **Tasks** are charged as a process is made and a thread's id chosen, and
  given back at reap.

The tree's root has no slot, so a machine with no limits set charges
nothing: under KVM a fault costs 848 ns against 832 without the quotas, and
a fork of 256 resident pages 79 µs against 77, within the spread of the runs.
The heap's charges cost a program in the root job a look at the running
task's job at each site: an open and close 4,104 ns against 4,044 on
`main`, a tmpfs create, 4 KiB write and unlink 12.7 µs against 12.2, and a
64-byte pipe write and read nothing measurable; in a limited job 4,139 ns
against 4,003 and 13.9 µs against 13.2 (best of seven, three boots each,
alternated, under KVM). Every boot's `quota` line drives each limit to its
refusal and back to zero, and its `kmem` line fills a limited job with each
kind of heap in turn until `ENOMEM`, with a sibling unrefused and every
byte given back.

### 1.7 The audit record — F-21b

The audit record ([AUDIT.md](AUDIT.md)) allocates nothing: its two rings and
the boot's pinned records are 288 KiB of static storage on every
architecture, and a record is a 64-byte copy and two counters under a leaf
`IrqSpinLock` (§1.1's count is unchanged). A record is made only where the
TSF decides something -- a refusal, a grant, an end, a quiesce, a change --
never on the path of a call that succeeds.

What a record costs, as every boot's `audit` line measures it on a store of
its own, 4,096 records of each kind, 2026-09-27:

| | kept (a grant) | counted past its budget (a refusal flood) |
|---|---:|---:|
| x86-64, KVM | 21 ns | 22 ns |
| x86-64, TCG | 115 ns | 130 ns |
| AArch64, TCG | 277 ns | 1,047 ns |
| ARMv7-A, TCG | 233 ns | 236 ns |

KVM's figure is the one to read: TCG's are emulation, and its AArch64
exclusive-monitor loop makes the refusal's second lock the slow one. A flood
costs no more per refusal than a grant, since past its 64 a second a refusal
is a counter bumped under the ring's lock, not a record written.

---

## 2. Worst-case execution time — F-24

### 2.1 What is claimed

Nothing, and deliberately. `docs/ARCHITECTURE.md` §5 states it plainly: the
`HardRt` domain *"does not promise a certified worst-case execution time for
the whole kernel — no OS that also hosts LLVM can offer that, and claiming it
would be the kind of statement that gets believed."*

That sentence is correct and this section exists to give it consequences rather
than leave it as a remark in a design document.

### 2.2 What the item does promise

Real, and narrower than a WCET:

* **Admission control.** EDF with a CBS test that refuses an unschedulable set
  — a bound on *acceptance*, not on execution.
* **Partitioned scheduling** in `HardRt`, no work stealing, which is what makes
  the admission test valid at all.
* **Bounded critical sections on the RT path**, by construction rather than by
  measurement.
* **Preemptible kernel**, so a long section delays rather than blocks.
* **Interrupts that cannot steal unaccounted time.** A line the controller
  holds (level-triggered, or of unknown trigger) is masked from each delivery
  to its acknowledgement, so it runs its handler at most once per
  acknowledgement. An edge-triggered MSI-X vector is no longer masked per
  delivery (L.object.41, since 2026-10-01). It is masked only after
  `STORM_BOUND` = 64 deliveries without an acknowledgement, and stays
  masked until the next one. Acknowledgements come only as fast as the
  holder's task is scheduled. So one MSI-X line's interrupt load is at most
  64 handler runs per scheduling of its holder, where it was 1 before; the
  65th delivery masks the entry and is counted in `Line::storms`.

  Who pays: the handler's time is charged to whichever task the interrupt
  cut. A storming device whose driver acknowledges promptly therefore costs
  other partitions up to that bound. The time is accounted, but in the
  victim's account, not the device holder's. AoU-4 and ASR-8 rest on this
  sentence, and it is the reason the bound is a named constant with its own
  check (the `irq` line) and its negative control.

### 2.2a The one bound the item puts on its own waiting

A processor that interrupts the others and waits for each to answer -- a TLB
shootdown or a grace period, `src/kernel/src/smp.rs` `wait_for` and `take_turn` --
gives up and stops the machine (FX-0001 to FX-0003) if one never answers.
That bound is a liveness diagnosis, not a safety property: waiting longer never
frees memory early, so its one job is to report a processor that will never
answer without ever calling a live one stuck.

Until 2026-09-26 it was one second of wall-clock time, and that measured the
host rather than the guest. Under QEMU's coverage plugin, which runs every
translated block through one process-wide lock, `test-compositor` stopped on
FX-0001 with nothing stuck. The bound is now two conditions together: the
wall-clock floor it always had, and a count of the waiter's own polls, which
slows exactly as the machine does and stands still while the waiter is not
running. A processor made to stop answering is still found: in 1.8 s under
KVM, 5.1 s under `tcg` and 32 s under the plugin. The residual is a host that
starves one virtual processor while it runs the waiter; that ends the wait
early, which costs availability and never integrity.

### 2.2b The filter a program supplies (seccomp, landing S2)

Every system call of every program now passes a function the personality
registers with the core (`trap::filter_system_call`), and from landing S3 that
function may run a program the user wrote -- classic BPF, in ring 0, on the
call's own path. What a filter can do to a call is bounded by the core: the
call goes on, or it fails with an errno the core cuts to 4,095. It cannot
return another value, start a program or choose registers.

**The bound on time.** Linux's, and the verifier's and the chain's enforce it:
a filter is at most 4,096 instructions, every jump goes forward (so it runs at
most its length), and the filters one thread holds may total 32,768
instructions, each counted with four more (`MAX_INSNS_PER_PATH`,
`ferrix_seccomp::fits_path`). So **one call runs at most 32,768 interpreter
steps**. The crate's constant was `1 << 18` in S1, eight times too large; it is
32,768 from the crate's own correction, the one S3 enforces at install, which
refuses a filter that would pass it with `ENOMEM`.

**What a step costs, measured.** The boot reads it (the second `seccomp` line):
the longest program the verifier admits, 4,095 loads of `seccomp_data` and a
return, run 500 times against a call's data, in the dev profile in QEMU on the
development host.

| Architecture (accelerator) | one hook, no filter | one interpreted instruction | 32,768 steps |
|---|---:|---:|---:|
| x86-64 | 17.1 ns | 27.2 ns | 891 us |
| AArch64 (TCG) | 15.0 ns | 25.8 ns | 845 us |
| ARMv7-A (TCG, two processors) | 37.9 ns | 60.2 ns | 1,972 us |

A virtual machine's clock is not a bound, and these are emulated processors, so
the figures are read and not judged; they say the worst case is below two
milliseconds on the slowest machine measured. Chromium's filters run a few dozen
steps for a call.

**AoU-4.** A program a user supplies may therefore take up to 32,768 steps on
every call it makes, and the hook is written for it: it allocates nothing, takes
no sleeping lock and reads registers only; a thread with no filter pays one
load of the registration, an indirect call and one load of the boot check's probe
word (and, once any thread has held a filter, one look for the running thread). The hook is entered and left with interrupts masked, as the
core's entry holds them (S2's registered body is a load and a store of a flag,
and runs masked). S3's body, which runs a chain, opens interrupts for exactly
the walk of the chain and closes them before it returns, as the dispatcher does
around the call it serves, so the up to two milliseconds above are preemptible
and are never added to the item's masked time. If a later body ran a chain
masked, that time would be added to AoU-4's budget.

### 2.3 What is missing, per standard

* **DO-178C DAL C** does not require WCET as such, but does require that
  timing-related requirements be verifiable. The item has no stated timing
  requirements to verify, which is a symptom of F-15.
* **EN 50716 SIL 2** expects timing behaviour to be analysed where a safety
  function depends on it. No safety function is defined (F-20), so there is
  nothing to hang the analysis on.
* **ASIL D / SIL 3-4**, the ratchet's destination, would require it outright,
  and the `core` ring is where it would have to be attempted — the Linux
  personality and the filesystem are the parts that make it impossible, and
  they are outside the item already.

### 2.4 An honest note on the boundary

The item boundary helps here more than anywhere else. A WCET argument over
93,646 lines including btrfs and a TCP stack is not a project anybody would
start. Over the 38,989-line `core` ring, with no dynamic allocation on the RT
path and no recursion anywhere (`tools/common/check/check-complexity.py` establishes the
second), it is at least conceivable. It has not been started.

**Verdict: F-24 stands**, now with its scope stated: no WCET is claimed, the
narrower guarantees that are claimed are listed, and the ring where an attempt
would have to be made is named.

---

## 3. Why F-24 is not closed, and F-23 is

A document that says "the property does not hold" is not a closed finding, and
recording it as one would be the exact failure this directory exists to avoid.
F-24 is restated against this analysis rather than struck out.

F-23 closed on 2026-09-26 because what it described stopped being true, and a
gate and a boot check say so: no allocation in the item's product code is
fatal except at bring-up. What it did not describe, a bound, stays unclaimed,
and says so in §1.5.
