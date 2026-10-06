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

**Pid 1's inputs allocate nothing (2026-10-04).** The program init starts,
its `sh -c` script and the list of commands come from the boot initramfs
under `.ferrix/init/` (`init::set_inputs`, ITEM.md §2). They are not copied:
each is a `&'static` slice of the archive `fs::init` keeps for the whole boot
(`fs::ARCHIVE`), and `init.rs` keeps three such slices in a `Once`. Judging the
entries is a three-slot array on the stack and a pass over an iterator, so
an archive with any number of entries under `.ferrix/` costs time in
proportion to them and no memory; each refusal is one console line.

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
item's own source is reported, not fatal, and the build says so. That was the
kernel's part of the item; the btrfs crates, which joined it on 2026-10-02,
were converted the same way the same day (§1.8). Two item
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
  routing tables, a btrfs transaction's changed nodes and the dentry cache's
  queue (made whole at boot and never grown) are the machine's, each with a
  fixed bound; a VMO's page list is a few dozen bytes per
  charged frame. `memory.stat`'s `kernel` line reads the heap alone.
  System V shared memory's `shmget` scans every slot of its IPC namespace's
  table under the table's spin lock -- for a key, and to count the job's
  segments and reserved pages -- and a table's slots never shrink, as the
  semaphore table's do not: the time is linear in the most segments the
  namespace ever held at once, at most 4,096 a job (`syscall/shm.rs`).
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

### 1.8 btrfs — the reader and the write path

The btrfs crates joined the item on 2026-10-02, and what §1.1 to §1.5 say of
the item's own source holds for both: the fallible-allocation gate counts 0
allocating calls in either that cannot report failure, and `cargo xtask
check` holds them at 0 (`L.btrfs.22`).

**The reader allocates nothing.** `ferrix-btrfs` is `no_std` without
`alloc`: its chunk map's storage, its node buffer and the buffers a file
read expands extents into are lent by its caller (`ChunkStorage`,
`ExtentBuffers`), and a volume with more chunks than the map holds is
refused with `ChunkMapFull`, not grown into. Its memory is what the caller
lends: one node (at most 64 KiB), two extent buffers of 128 KiB and zstd's
workspace, at the size the kernel's glue picks.

**The write path allocates, and reports failure.** `ferrix-btrfs-write`
holds the running transaction in memory -- every node it copied (`dirty`),
the delayed reference changes, each block group's free, pinned and reserved
ranges -- and a cache of nodes read and unchanged (`clean`). Every one of its
allocations goes through its `fallible` module, over `ferrix-fallible`, and a
refusal is `Error::OutOfMemory`, which the VFS glue answers `ENOMEM`:

* a vector is reserved with `try_reserve` before it grows, as in §1.1;
* a `BTreeMap` or `BTreeSet` insert runs in a reserved section, as the
  kernel's own do: a library crate cannot name `mm/reserve.rs`, so
  `ferrix-fallible` dispatches to a section the kernel installs at boot
  (`fallible::install_library_sections`, right after the boot processor's
  reserve is filled). One insert per section; a map is copied one insert
  at a time. A map whose nodes are larger than the largest size class is
  refused at run time -- the kernel's own maps are checked at compile time
  -- and a host test holds every map the write path keeps under it
  (`L.btrfs.116`). The boot's allocation check drives the section with the
  heap bypassed, with its reserve refused, and with an over-class map
  (`L.mm.63`, `L.mm.64`).

What a failure does to the transaction: inside an edit -- every tree edit,
and so every commit -- it aborts the transaction, even before the edit's
first change, and the volume reopens at its last commit; in an operation's
own code before its first edit it changes nothing. Nothing allocates after a
commit's primary superblock is written, so a failure is never reported for a
commit that is on the device (`L.btrfs.108`, `L.btrfs.113`; `H.STORE.7`). The
reader still allocates nothing; a device whose read runs out of memory
answers `OutOfMemory`, which the reader and the writer pass on without
reading another copy (`L.btrfs.110`, `L.btrfs.111`). Its 19 calls the gate
counts by name are arguments now (`NOALLOC:`): methods of its own on
caller-supplied buffers. What the heap's exhaustion still does is AoU-5's:
the transaction aborts, so the mount reloads read-only at its last commit,
and every partition's uncommitted writes on it go (VULNERABILITY-ANALYSIS.md,
T.EXHAUST).

**What bounds it, and what does not.**

| What | Bound | Where it is set |
|---|---|---|
| Clean nodes cached | 4,096 nodes, then dropped (`CLEAN_NODES`): 64 MiB of 16 KiB nodes, before the in-memory form's overhead | the item, `volume.rs` |
| Nodes a transaction holds (`dirty`) | none in the item; the glue commits once `dirty_bytes()` or the data written reaches 32 MiB (`COMMIT_THRESHOLD`) | the load, `ferrix-btrfs-vfs` `rw.rs` |
| A writeback's copy buffer | 256 pages, 1 MiB (`WRITEBACK_PAGES`) | the load, `rw.rs` |
| Delayed references, ranges per block group | none: they grow with the transaction's edits and the volume's fragmentation | -- |
| A log replayed at mount | none: proportional to the log the last mount left | the item, `log.rs` |
| The commit's settling loop | 64 passes (`SETTLE_PASSES`), then `Inconsistent` | the item, `commit.rs` |

So the write path's memory is bounded only by the load's commit threshold,
and between commits by nothing the item sets; a job driving btrfs writes is
charged for its pages, not for the transaction's nodes (V-05's "a btrfs
transaction's changed nodes").

**Space on the volume** is a separate budget with its own reserves, kept so
that a full volume refuses before it changes anything (`H.STORE.6`): an
operation needs room for its own worst case, 64 nodes (`EDIT_RESERVE`), and
a node per MiB of data it writes (`Need::data`); the commit's, a node per 16
it has changed plus 128 (`commit_reserve`); and, unless it frees space, a
global reserve of 256 nodes (`GLOBAL_RESERVE`) a deletion may use. The
commit's share and the data write's are **estimates**, shown sufficient on
the 128 MiB fixture only: deriving them, and testing a full-size
transaction on full trees, is open (`docs/BACKLOG.md`, TODO.md §4.7 item 5).
The first 1 MiB of the device is never allocated (`DEVICE_RESERVED`), nor is
any superblock copy's stripe (`L.btrfs.16`, `L.btrfs.92`).

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
  Taking and letting go of a kernel spin lock masks no interrupts on x86-64:
  the preemption count it raises is one `GS`-relative `xadd` in the
  processor's record (L.sched.20, since 2026-10-03). On AArch64 and ARMv7-A
  the raise and the lower each mask interrupts for a load and a store of that
  record, a few instructions, where both used to mask for two locked
  operations.
* **Interrupts that cannot steal unaccounted time.** A line the controller
  holds (level-triggered, or of unknown trigger) is masked from each delivery
  to its acknowledgement, so it runs its handler at most once per
  acknowledgement. An edge-triggered MSI-X or MSI vector is no longer masked
  per delivery (L.object.41, since 2026-10-01; MSI since 2026-10-02).
  An MSI vector of a function with no per-vector mask bit is masked by MSI
  Enable, which drops the device's messages until the acknowledgement
  rather than deferring them, as an MSI-X entry's pending bit would
  (AoU-19). It is masked only after
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

A program leaving its speculation domain (`speculation::leaving_domain`,
SPECULATION.md §3) waits for one grace period under this bound, and holds
one of eight slots in the set of domains being left for that long, which
each processor answering compares its own last domain with (F-60). A ninth
leave at once yields until a slot is given back, each slot being held for
one grace period; no order among such waiters is promised, so this is a
bound only on what each holder takes, not on a waiter's place. Leaves are rare -- a move between jobs,
a loss of dumpability -- and each answer of a grace period reads the eight
slots once, with interrupts masked.

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

**What S3's chains cost, measured.** The boot reads, in the guest and in the
dev profile, the two worst chains the limits allow: 6,554 filters of one
instruction (most runs of the interpreter, each with its loop and `Arc`
deref) walked in 401 us a call, and seven filters of 4,096 instructions
(most steps) in 732 us a call, both on x86-64 where a step is 25 ns; and the
release of the 6,554-filter chain, which is a walk of `Drop`s and runs under
no lock but where the last reference goes, 8.1 ms. A process's chain is
released by its last thread or child, and in production the last drop can fall
to the reaper with preemption off: **that is up to 8 ms of masked-preemption
time in the reaper for the worst chain a program can build**, and is stated
here rather than argued small. Every other release (a chain of a few filters,
which is every program in practice) is microseconds. S3 does not bound it
further; a deferred release by the reaper's own work queue would, and is a
BACKLOG row.

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

### 2.2c A channel round trip, measured, with and without a speculation domain

Measured, not bounded, on branch `os-ipc/zircon-trip`, which carries what
`docs/OPAQUE-KERNEL.md` §9.4 lists beside the domain: its `cargo xtask
bench-ipc` times 20,000 round trips of an eight-byte message between two
native processes, each side making one `channel_write_read` (0x1013), which
that branch adds. The figures below were taken at commit 5658e7c0c of that
branch, the domain's code before the review of §9.3b, on x86-64 under KVM on
nazuna, a Zen 5 host, at host loads of 7 to 15, built `--mitigations on`, at
p50. The benchmark keeps its counts in buckets an eighth of a power of two
wide and reports a bucket's floor, so a percentile is good to about 12% and
can read below the exact minimum it also prints (2,866 ns minimum against a
2,789 ns p50, two processors, in a domain):

| | one processor | two processors |
|---|---|---|
| a write, a wait and a read on each side | 13.0 us | 13.0 us |
| `channel_write_read`, the two in no domain | 6.5 us | 7.4 us |
| `channel_write_read`, the two in one speculation domain | 3.0 us | 2.8 us |

The difference between the last two rows is the predictor invalidation at
the two switches a round trip makes. A domain removes it, and it is the one
part of the switch a domain changes (SPECULATION.md §3). The p99 is three to
four times the p50 on a shared host. These are measurements of one
configuration, not a bound.

**From step 3 on (OPAQUE-KERNEL.md §9.8, 3a and 3b), a figure runs with the
vector-state contract.** A task switched away blocked in `channel_write_read`,
`object_wait_one` or `port_wait` keeps only `MXCSR` and the x87 control word
and is given the initial vector state back, and the switch reads no `FS` base
MSR. Every round-trip figure from that commit on is of that configuration,
and says so beside it. Its first, with the exact `bench-ipc` (sorted samples,
one pinned processor, `--alternate` turn about), x86-64 under KVM on nazuna,
one processor, `--mitigations on`, in one speculation domain: `domain-call`
p50 2,397 ns against 2,546 ns without step 3, five rounds, ratio 0.945
(`~/.local/share/ferrix/logs/queue/po6-ipcB-s3-bench-2.log` on nazuna, on
4a8b8dfee; §9.9 carries the figure retaken on the landed base).

Two decisions the round trip's wakes defer are bounded, though no check
times them (L.sched.7). A wake made inside a system call asks for no timer:
the decision it wants is made when the call ends (`sched::call_left`), at
the next `preempt_enable` that brings the count to zero, or at the next
interrupt's exit, whichever comes first, and the running task's slice timer
is still armed. A `Wake::Sync` waker that does not block after all shares
its processor with the task it woke until the next decision, which the
woken task's arrival arms the timer for at most one slice away: the woken
task waits at most one slice, inside H.SCHED.2's bound of one slice plus
the timer overruns served.

The timer's skipped arm (`timer::after`, L.sched.5) adds no lateness: what
it keeps is the clock read after the hardware was armed plus the delay, an
upper bound on the interrupt, and a request is skipped only when that bound
is no later than its own deadline. The one lateness left is the hardware's,
a delay shorter than one timer tick armed as one tick, which every arm has.

### 2.2d btrfs

No time bound is claimed for the btrfs crates either, and their time is
spent inside the load's calls, under the load's locks (CLAIM.md §3.1 (c)).
What bounds the work in them:

* **A lookup** descends at most eight levels (`MAX_LEVEL` 7), each a node
  read, a CRC-32C over the node, and a binary search; a damaged copy costs
  one more read per copy the chunk keeps. **A walk** is linear in the keys
  it visits, which ascend strictly. **A compressed extent** expands into at
  most 128 KiB.
* **A commit** settles in at most 64 passes, each linear in what the
  transaction changed, then writes each changed node to each of its copies,
  flushes, and writes the superblocks, the primary made durable by a second
  flush or a FUA write. How long a flush takes is the device's, and nothing
  in the item bounds it.
* **Opening a volume for writing** reads the chunk, root and block-group
  trees and the free-space tree, and replays whatever log the last mount
  left, unmeasured on a full volume (`docs/BACKLOG.md`).
* **Interrupts masked.** Each map insert of the write path runs in a
  reserved section (§1.8), so this processor's interrupts are masked for
  the section's entry -- topping the reserve up to 16 objects of each size
  class when a section before it drew on it -- and for one insert, which
  makes at most height + 2 nodes of at most 2 KiB each. A commit makes many
  such inserts, each its own window, all in task context under the volume's
  sleeping lock; none waits. This adds to the masked windows AoU-4 budgets.

### 2.2e The IOMMU's bridge table (`iommu::BRIDGES`)

A spin lock over the bridges PCI enumeration found, written only at stage
10 (`iommu::learn_bridge`, one push per bridge) and read from then on. It is
a leaf: nothing is taken under it. The longest hold is a function's first
domain (`iommu::vtd_unit_for`, under the device node's domain lock): for
each of the unit's kept DMAR scopes, a path of at most 124 hops each
matched against every bridge, and then up to 256 steps up the bus numbers,
each a pass over the bridges -- at most scopes × (hops + 256) × bridges
comparisons, a few thousand on any machine Ferrix boots, with no
allocation and no wait. Placement at boot (`iommu::discover`) holds it the
same way once per function.

### 2.2f Device memory types (`user::memory_type::CLAIMS`)

A spin lock over the physical ranges user mappings of device memory hold,
each with its memory type (device, write-combining or cached) and a count
of the mappings holding it, so that no page is mapped with two types
(L.user.109). It is a leaf: taken under an address space's lock by
`map_device` and `map_window`, and on its own when a mapping's hold drops
after its unmap's shootdown; nothing is taken under it.

**The bound, which ring 3 cannot raise.** There is one entry per distinct
range and type, counted, never one per mapping, and the range is not the
program's to choose: an I/O mapping holds its whole aperture, and a
render node's window holds its whole blob whatever part of it an `mmap`
asked for. Live blobs are disjoint whole pages of the device's
host-visible window and stay placed while anything maps them. So the
entries are at most the whole-page apertures stage 10 minted plus the
pages of the host-visible windows -- fixed by the machine's hardware. Any
number of processes mapping any number of times raise counts, not
entries. A hold is one pass over the entries comparing two bounds each,
and at most one fallible push (`fallible::try_push`, `NO_MEMORY` to the
caller); a drop is one pass and, at a count of zero, a `swap_remove`.
There is no wait under it. Stage 9 checks that mapping a window again,
whole or one page of it, adds no entry.

**A residual.** The hold orders translations, not caches: after the last
cached mapping of device pages goes, dirty lines may be written back
after a later write-combining or uncached mapping wrote those pages. No
two types are mapped at once, so the effect is stale data over the
device's own memory, not a machine-wide one (VULNERABILITY-ANALYSIS
T.DMA path 7). A flush when a cached hold is released would close it.

### 2.2g The pin quarantine's lock (`object::pin::QUARANTINE`)

A spin lock over the list of quarantined pins and the pages raised pin
budgets hold of the ceiling. Since NVIDIA's N0f it is also the lock every
device's pin budget and its three counts -- `live`, `quarantined`, `kept`,
kept on the device's domain -- are read and changed under
(`L.object.118`–`120`). It is a leaf: nothing is mapped, unpinned,
allocated, waited for or locked under it. A pin's reservation
(`object::pin::reserve`) and its give-back, a closed pin's move to `kept`,
a release's count per entry and `device_set_limit`'s test and store are each
a handful of loads and stores under it, with no walk: the pin path no
longer walks the quarantine list to count a device's pages, as the cap did
before. What still walks the list is the release, once, to take a device's
entries off it -- a pass over every quarantined pin on the machine, which
the budgets bound at twice each device's budget in pages, with no
allocation; the unpins and the frames' release come after the lock is
dropped. A quarantined pin's own record is allocated with the pin, outside
the lock (finding F-23), so a death allocates nothing.

### 2.2h A device's configuration lock (`DeviceNode::config`)

One interrupt-masking spin lock per device node, held across every write of
a published PCI function's configuration space (`L.object.117`): bus
mastering's read-modify-write of the command register, an MSI mint's
`INTx` off and its message (`Msi::program`: at most seven accesses), an MSI
vector's mask (one read and one write, from an interrupt handler), MSI-X's
enable and function mask (a read and a write), a driver's
`device_config_write` (one write), `verify_config`'s read-back -- the
command register, at most twelve BAR dwords and, with MSI, five more reads --
and a refusal's (a read and a write of the command register).
It masks interrupts because an MSI vector is masked from interrupt handlers.
It is a leaf: nothing is mapped, allocated, waited for or locked under it.
The function's configuration space is mapped once, before the lock is first
taken, and kept; an MSI mint takes it inside the vector's own `minted` lock,
which is the order (`minted`, then `config`). So the longest hold is about
twenty uncached configuration accesses, a few microseconds on any machine
Ferrix boots, with interrupts masked on the holding processor; it adds to the
masked windows AoU-4 budgets.

What the window keeps per node is made with the node at stage 10 and never
grows: the allowlist, one bit per byte of the 4 KiB (512 bytes, and 128 more
naming up to 32 capabilities), and the record of refusals printed, one bit
per dword (128 bytes), which `device_config_write` reads and sets only under
the lock and prints from after it is dropped. A refused write allocates
nothing.

### 2.2i A VT-d unit's invalidation queue (`iommu::vtd::Unit::submit`)

Since NVIDIA's N0g every VT-d invalidation goes through the unit's
invalidation queue (`L.iommu.47`): one descriptor and a fenced wait
descriptor, written into the unit's queue frame, cleaned to memory on a unit
whose walk does not snoop, then a write of the tail register, and a wait for
the wait descriptor's status word to read this submission's sequence
number. Memory: three 4 KiB kernel frames per unit -- the root table, the
queue (256 descriptors of 16 bytes) and the status word's frame -- taken
once at stage 10's bring-up through `vtd::table` and never given back, and
on a unit that remaps interrupts a fourth, its interrupt remapping table
(256 entries of 16 bytes). No domain maps them. A submission allocates
nothing, nor does writing an interrupt entry: its index is its vector's
slot among the sixty-four the architecture hands out, taken by the existing
compare-and-swap on `TAKEN`, so remapping needs no allocator and no lock of
its own, and an entry, like its vector, is never given back.

Time: a submission is held inside the unit's `commands` gate (`gate.rs`), a
gate and not a lock, entered with interrupts on wherever the caller may
block, so the wait gives up the processor between looks as the register
wait it replaces did. The hold is two descriptor writes, their clean, one
register write, and the wait, bounded by the unit's patience, 100 ms. The
order with a domain's `changing` gate is unchanged: `changing`, then
`commands`, never the reverse. An MSI or MSI-X mint enters `commands` under
its device's `minted` lock, to write the vector's interrupt entry and wait
for its invalidation: there the gate spins with its deadline, the unit's
patience, with interrupts masked on that processor where `minted` is an
`IrqSpinLock` -- a hold bounded by one entry write, its clean and one
invalidation, at stage 10 and when a driver first asks for a vector, never
per interrupt. The console's I/O APIC line takes its `CONSOLE_INPUT` lock
twice at bring-up, once to mask it and once to write its remappable entry,
and the entry and its invalidation are made between the two, never under
it. The check vector 0xFC's handler only counts and acknowledges.

A failed invalidation -- the status not written within the patience, or
`IQE`, `ICE` or `ITE` seen -- answers an error and its caller releases
nothing (`L.iommu.48`), so memory the unit may still reach stays held for
good and counted in the device's `kept`. After `IQE` or `ITE` the unit is
marked failed: every later invalidation on it fails at once, without
entering the gate or touching the queue, so a broken unit costs nothing
more than holding what its domains held, and the boot's fault audit fails
on it (FX-1007). A completion error (`ICE`) is cleared and counted as it is
taken, so it fails one invalidation and never every later one; queue errors
firmware left are cleared at bring-up, or the unit is refused. Check R7 makes
one invalidation fail on purpose with a
patience of its own, 2 ms, rather than spend the unit's 100 ms of every
boot on an answer known in advance.

### 2.2j The way back to user mode and a process's task list (`sched::work`)

Every return to ring 3 makes one masked look at the running task's
pending-work word (`L.sched.30`): the running task's run-queue lock, held for
one load, and, only when a bit is set, one `Acquire` read-modify-write to
clear `STOP` and `SIGNAL`. The personality's `needs_attention`, with its two
references and two locks, runs only after a set bit -- or, with the
self-checks on, at a clear word too (`L.sched.33`), which is the cost every
return paid before 2c. So the way out's span shrinks only in a boot run with
`ferrix.checks=skip`; the default boot keeps the old cost and the check. And
no saving is claimed for 2c until 2a lands: `look` still takes the run
queue's lock to find the running task, which costs about what the two
references it replaced did.

The core's end (`object::process::Process::end_record`, `L.sched.35`) and
every walk that posts a bit to a process's tasks (`post_to_tasks`) allocate
nothing: the tasks are taken out of the list eight at a time onto the
walker's stack, under the list's interrupt-masking lock, posted to there, and
woken and released after it. A walk that finds the list changed since its
last batch starts again from the top, so its length is bounded by the
number of starts the process makes while it is walked. That number is
bounded too: a start lists a task of the ending process itself, made by a
thread of that process, and every such thread has `END` posted by the walk
or, listed after it, by its own start, so it leaves at its next way back to
user mode instead of starting more; and a start that reads the end already
recorded refuses (`start_thread`). Listing a task
(`list_task`) is the one allocation, `fallible::try_push` before the task
can run, and a start whose task cannot be listed fails before anything runs
(F-23).

### 2.2k The chardev core's tables (`interfaces::chardev`, load ring)

NVIDIA's N1e forwarding core (`docs/NVIDIA.md` §4.4) is in the `load`
ring, outside the item; the item gains only its five call numbers and their
`SERVED` rows. Its bounds are written here because the consultant made them
a condition of landing it (ledger 294, N10). They hold on every image, and
the core does nothing on any image but `run-nvidia`'s: a control is made only
for a device devmgr has handed over isolated.

Memory. A control is made by `chardev_control_create`, one per device
(`CLAIMS`), and everything it will hold is reserved then: the request table,
a list of at most `MAX_OUTSTANDING`, 256, requests sorted by id, and the
queue of messages for the driver, both `try_reserve_exact` for 256 entries,
and the control itself through `fallible::try_arc`; a refusal is
`NO_MEMORY` with nothing kept. Each open of a node reserves one more queue
slot for its release (`hold_release`, `ENOMEM` on failure), so queuing a
release allocates nothing and cannot fail (N8). A request is one fallible
allocation (`ENOMEM`); the 257th outstanding request is `EBUSY`.

**The queue's bound (F-63, closed 2026-10-05).** A request leaves the
table when it is answered or abandoned, which may be before the control's
task has written it to the driver, so the table's count alone does not
bound the queue. The control counts the requests in its queue (`queued`),
and admission refuses `EBUSY` while either count is at 256. An abandoned
request still queued is taken out of the queue at once (a search of at most
256 requests and the releases held, under the lock), and the task skips a
request answered or abandoned before its turn. So the queue holds at most
256 requests and one release per open file, the room reserved for it, and
its `push_back` never allocates. Stage 10's chardev check drives a driver
that reads nothing through rounds of requests abandoned, and answered
before their turn, and requires the bound; its controls are in F-63.
The global lists are bounded by what they list: `STARTING` and
`CONTROLS` hold one entry per control, at most one per device, and
`PUBLISHED` one per published minor, unique system-wide and at most 256
(minors 0 to 255, at most eight a HELLO). A copy's bounce buffer is one
fallible heap allocation of at most 4 KiB per call, never kernel stack.

Locks and time. Nothing is copied, waited for or mapped under a lock of the
core, and no lock of the core is taken under another. The one lock taken
beneath them is the heap's, an interrupt-masking spin lock, through the
fallible allocations named here: a request's record and an open's
reservation under the control's `state`, and the lists' room under
`PUBLISHED`, `CONTROLS` and `STARTING`.

* A control's `state`: a request's admission (the count, the id, one
  allocation, two pushes into reserved room), a reply's or an
  abandonment's lookup by binary search over at most 256 ids, and its
  removal, which moves at most 255 pointers; an abandonment's search of the
  queue, at most 256 requests and the releases held; a release's push; an
  open's reservation, which may allocate.
* A request's `inner`: a handful of loads and stores (`alive`, `answer`,
  the count of copies in flight).
* `CONTROLS`: a reply's or copy's search for the control whose driver end
  the handle names, one pointer comparison per live control.
* `PUBLISHED`: a HELLO's uniqueness test, at most 8 by 256 comparisons, and
  an open's lookup of its minor, at most 256.

No copy runs under a lock. A copy call moves at most 1 MiB, 256 chunks of
4 KiB, through the bounce buffer; between chunks it takes the request's
lock to read that the request is still alive and unanswered, and drops it
before the next chunk. The copies are plain user accesses: `main` has no
fault a ring-3 driver serves, so a copy cannot wait on `nvrm`. When fault
windows land the copies must switch to the mode that refuses a window
(ledger 294's N5, a `docs/BACKLOG.md` row).

Waiting. A client's open or ioctl sleeps until its driver answers,
with no deadline, as a call to any ring-3 driver does; a signal ends the
wait with `EINTR`, after which the request is dead and its client waits out
at most one chunk of a copy already in flight (N4, the drain), and the same
drain follows an answer (L1). A release does not wait: it is
queued and the client goes on. These are waits on a driver, not work of the
item, and no bound is claimed for them. The driver's HELLO is waited for at
most 10 s by the control's own task, never by a client or the boot.

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
