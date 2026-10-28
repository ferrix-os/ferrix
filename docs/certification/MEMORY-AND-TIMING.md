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

**devmgr's DEVICES buffer is on the kernel stack (2026-10-17, F-82).**
`discovery/devmgr.rs::write_devices` encodes each DEVICES message into one
array on the calling task's kernel stack before copying it to the heap
`Vec` the channel takes. It runs at bring-up (`start`) and, under
`ferrix.devmgr=init`, from `devmgr_start`'s system call (`start_in`). Its
size is `DEVICES_HEADER_BYTES + MAX_DRIVERS x NAME_BYTES`, 24 + 32 x 32 =
1,048 bytes at `MAX_DRIVERS` 32 (536 at 16), on a 16 KiB kernel stack
(`vmap::STACK_PAGES` 4). It is not nested or recursive, and
`write_devices` holds no lock of its own while it is live. All three
constants are `ferrix-devmgr-proto`'s, a crate the item does not classify
(F-76's class), so a review of `MAX_DRIVERS` or `NAME_BYTES` is a review of
this frame. A const assertion in `devmgr.rs` holds the buffer at 1,048
bytes, so that growing it is a diff to the core file as well.

**The kernel log's size, `log_buf_len=` (2026-10-23, ledger 3366).** The
log starts as two static arrays, 128 KiB of bytes and 4,096 line slots of
16 bytes (`console::log::LOG`, `LOG_LINES`). With `log_buf_len=` asking for
more, `console::log::size_from` takes at most 4 MiB of bytes and 2 MiB of
line slots from the heap, fallibly, once at boot (`main.rs::size_log`,
before the second processor, interrupts masked), and never frees them:
they are the log for the rest of the boot. A size that is no size, or no
memory for it, is one console line, and the 128 KiB log is kept.

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

*`/proc/<pid>/maps` no longer stops it* (F-97, closed 2026-10-27). Its text
was rendered at open into one `Vec<u8>` grown infallibly, and a single
request past the heap's largest block (4 MiB, order 10) fails however much
memory is free: an unprivileged program with enough regions, or long
enough mapped paths, stopped the machine by reading its own maps, and no job
limit helped, since the snapshot was charged only after the render. Now
each line is formatted into room reserved for its longest form, and the
text is kept in page-sized blocks (`ferrix_fallible::Chunks`), each charged
to the opener's job before it is taken; a refusal answers the open
`ENOMEM`. mountinfo, mounts, `/proc/net/tcp`, `udp` and `unix`, and the
cgroupfs listings still render into one buffer (F-97's residual, F-98).

**The kernel's watermark** (`H.QUOTA.13`, L.mm.71 to 74; since 2026-10-13).
The heap takes its pages from the same frame allocator as a program's
memory, so a program in a job with no limit could take the last free frame,
and the next infallible allocation in the load stopped the machine: an
`epoll_wait` did, with FX-0008, when Wine's own Direct3D used up an 8 GiB
guest (endfield-s3's control, `~/.local/share/ferrix/logs/queue/endfield-s3-ctl-wined3d.log`).
Since then a program's frames, its runs of frames and its page tables are
refused when taking them would leave fewer than the watermark free, decided
in the same hold of the frame allocator's lock as the take
(`mm::allocate_program_frames`); the kernel's own frames and the heap's
pages are not. A refused program frame takes the path a machine out of
frames always took: clean cache reclaimed first, then the fault fails
(`SIGSEGV`, `EFAULT`) or the call answers `ENOMEM`.

* **Its size** is the frames managed at `mm::init` over 128, within 256 and
  8192 frames (1 to 32 MiB); the loader's and ACPI's memory reclaimed after
  `init` is not counted. It is an engineering margin, not a derivation, and
  it is fixed: not a boot option, which would widen the configuration space
  ITEM.md describes. The boot's `memory` line prints it.
* **It is a count, not contiguity.** A kernel allocation of more than one
  frame can still fail with the watermark's frames free, if they are
  scattered.
* **It bounds what programs take, not what the kernel takes for them.**
  Heap-borne demand -- the load's lists, a job with no limit's descriptors
  and sockets -- draws on the same frames below the watermark, and the
  load's infallible allocations still stop the machine with FX-0008 once
  those are gone too. What it buys is that page demand alone, a program's
  memory and tables, cannot get there.
* **Routes left outside it**, each bounded otherwise: an address space's
  root table (one per space, spaces bounded by the job's task limit);
  kernel stacks (one per task, the same limit); a display client's
  contiguous buffers (`interfaces/display`, at most the card's budget,
  `CARD_BYTES`, per card; a mapping kept past its buffer refaults zeroed
  pages into that buffer's VMO alone, up to its size, charged to the
  faulting job, not to the card; each head's mode list, at most 64
  entries of 28 bytes, twice over while the card's task builds the next,
  and at most 16 heads a card, reserved before any lock is taken,
  `docs/DISPLAY.md` §9.3); and every frame the kernel takes for itself, by `allocate_frames`.
* `/proc/meminfo`'s `MemAvailable` still counts the watermark's frames as
  available, where Linux leaves out its reserve (`totalreserve_pages`).
* **`memory.reclaim`'s rounds** (`user::cache::reclaim_asked`, 2026-10-26,
  L.object.183): each round is one `reclaim` of at most 32 pages, so its
  scan is bounded as every reclaim's is (two passes round the list of file
  objects), and no lock is held across rounds, each `Walk` dropped before
  the next. The rounds end at the count asked for, or after 17 rounds in all
  that gave nothing; a write of 1G is about 8192 rounds. An empty write asks
  for everything (`u64::MAX` pages) and, in a cgroup whose tasks keep
  filling cache again, ends only on a signal to the writer, which is looked
  for before every round: preemptible, and paid for by the writer, since
  the file is 0200 and the root has none.
* **The kernel's own users of the program routes**, which the watermark
  now refuses earlier than an empty allocator would, and why each is
  handled: a page-cache fill (`fs/pages.rs`, `allocate_reclaiming`), made
  by whichever task reads the file, kernel tasks at boot included,
  reclaims clean cache, in the job whose `memory.max` refused it or
  machine-wide, and tries again, at most 8 times while a reclaim gives
  pages back (since 2026-10-15: one reclaim let another processor take what
  it gave back, and a read answered `ENOMEM` with clean cache to spare), and
  its failure is the read's error. A machine-wide reclaim asks for one run
  and for what the free count lacks of the watermark (since 2026-10-17: at
  the floor the kernel's own allocations held the free count under the
  watermark by more than a run, so eight reclaims of a run each gave pages
  back and no retry found a frame -- a write answered `ENOSPC` with
  hundreds of megabytes of clean cache left, `docs/BTRFS-PRESSURE.md`).
  The bound: the watermark is at most `WATERMARK_MOST` (8192 frames), so one
  machine-wide reclaim steals at most 8192 + `MAX_FILL_RUN` = 8224 pages,
  and a frame takes at most 8 reclaims; the scan stays bounded as before by
  reclaim's two passes round the list of file objects, not by what is asked
  for, and no lock is held across a reclaim (`take_from` holds a pages lock
  per batch only). Since 2026-10-18 a reclaim takes the list's lock for one
  entry at a time, from the hand's place in it (`user::cache::object_at`),
  where it copied the whole list and upgraded every entry first: a reclaim
  that is satisfied in the first objects it looks at costs what it took, not
  one allocation and upgrade per cached file under the list's lock, which
  mattered once the dentry cache stopped bounding how many files are cached
  (below, the dentry cache). Each pass still goes once round the objects the
  list held when the reclaim began, as the copy did: a reclaim holds a
  `Walk`, and while any is held a registration never closes the list up (it
  only pushes at the end, the last walker closing it up as it leaves), so no
  entry changes place under a walk (ledger line 2336 (1)). A job's reclaim at its `memory.max` still asks for one run.
  Evidence: `gate.sh` logs `btp-pressure` (test-btrfs-pressure, x86_64
  KVM, passed) and `btp-ctl` (its control: the shortfall dropped,
  `No space left on device`, fired), ledger line 2314. The fill runs from a read and from a
  fault through a mapping (`Filler::fill`) under that same bound: one
  fill takes at most 32 frames (128 KiB), charged to the reader's job,
  and the 8-reclaim bound applies per frame; since 2026-10-15 a read on a
  sequential stream also waits for a whole run of 32 pages from the
  source, as a fault did, with no lock held while it waits (ledger line
  1700); a fault's
  own retry after it is `oom::out_of_memory`'s, so 8 is not the fault's
  whole bound; a file's write (`fs/pages.rs`,
  `write_reclaiming` over `Vmo::commit_and_write`, since 2026-10-15)
  reclaims the same way, in the job whose `memory.max` refused it or
  machine-wide, at most 8 times while a reclaim gives pages back, and then
  answers `ENOSPC`, as Linux's tmpfs does when it is full -- a memory
  filesystem's write finds nothing to reclaim and answers it at once; `Vmo::commit_run` behind `PIN_CONTIGUOUS`
  (`syscall/native.rs`) is a ring-3 driver's request and answers
  `NO_MEMORY`. Faults, copy-on-write and a fork's copies (`user/space.rs`,
  `user/vmo.rs`) are a program's own demand. `quota::charging_nobody`
  changes whom a frame is charged to, not which route takes it, and its
  one user, the console's descriptor table, takes heap, not frames. Every
  one of these callers already answered an empty allocator with an error,
  so refusing at the watermark adds no new failure path.

Where it stands: the consultant's design review is OK IF W1 to W11 (ledger
2026-10-13, line 797, `~/.local/share/ferrix/cert-consultant/reviews.md`);
the stage-2 check and its four negative controls are in the landing
(a7a32bf19), and the landing was accepted (ledger line 854). The same Wine
run at 8 GiB that stopped the kernel ended without it on 2026-10-13
(`endfield-oom-w2-unity-wined3d`: the game refused at 245 s, the kernel
ran on), so the BACKLOG row now holds only the residual: heap-borne
demand and the load's infallible allocations.

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
  Since 2026-10-26 (L.object.182) a program's page table is charged as
  memory and again as `Resource::Tables`, and its frame record carries
  `mm::TABLE_OWNER` beside the slot: its free takes back both, two walks of
  the job chain where a page's free makes one, still with no lookup and no
  lock, under whatever lock the freer holds, F-36's deferred frees
  included.
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
  bounds. A futex waiter is at most about 19 KiB on a 64-bit kernel since
  `futex_waitv` (2026-10-15): up to 128 table entries of 48 bytes each,
  with up to as much again of their buckets' growth slack (12 KiB), its
  words' list (128 of 40 bytes, 5 KiB), one bucket record per word (1 KiB)
  and its sleeper (56 bytes); the 3 KiB copy of the caller's vector is freed
  before it sleeps. Argued through the task limit, not charged, as before.
  A wake's walk under a bucket's spin lock is linear in that bucket's
  entries, now up to 128 per waiting task; setting up and leaving take one
  bucket's lock at a time, never two, and only a requeue holds two; 256 pseudoterminals, the neighbour and IP reassembly caches, the
  routing tables, a btrfs transaction's changed nodes and the dentry cache's
  queue (made whole at boot and never grown) are the machine's, each with a
  fixed bound; the dentries that queue lets go of while their files hold
  cached pages are kept beside it, by id, until reclaim has taken the pages
  or the name goes (since 2026-10-18, `ferrix_vfs::namespace`, `Paged`;
  `docs/BTRFS-PRESSURE.md`, "The page cache dropped whole"): each such
  dentry stays charged to the job that looked it up, as before, and its map
  entry, a few dozen bytes of uncharged heap, is the machine's -- at most one
  per cached file, and so at most one per cached page. The set's vector
  grows only with its lock let go, fallibly; a dentry it has no room for is
  dropped as the queue alone dropped it, and counted. Two of them are looked
  at again on each lookup that misses, and those whose pages are gone are let
  go. A reclaim that gave back fewer pages than asked also lets go of them,
  and of the queue's dentries nothing else holds, charged to its job or
  beneath it (`Namespace::shrink`, through the shrinker `user::cache`
  calls, 2026-10-26, L.object.186): F-90's residual, the release driven by
  reclaim, is done. One shrink holds the queue's lock at most
  `ceil(cache_limit / 32)` times, and in all looks along the queue once --
  up to `cache_limit` (4096) looks, each a walk up the owner's job slots by
  atomics (`takes`, which takes no lock), and up to 32 removals a hold, each
  a move of up to `cache_limit` entries; the set's lock is taken once a look,
  up to the set's size. No dentry is dropped under the queue's or the set's
  lock, and the two are never held together. An unmount holds the set's lock for one look at every kept
  dentry, each with a walk up its parents (`is_ancestor_of`): the kept
  dentries times their depth, with the room for what it takes out had before
  the hold (in holds of 64 when there is no room for all); a VMO's page list is a few dozen bytes per
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

The time figures below make no claim about the processor clock. They were
taken in QEMU, and on hardware with Intel hardware P-states, which the kernel
turns on since 2026-10-17 (L.x86_64.177), a core's clock may lie anywhere
between the Lowest and the Highest performance level it reports, below the
base clock firmware left included.

### 2.2 What the item does promise

Real, and narrower than a WCET:

* **Admission control.** EDF with a CBS test that refuses an unschedulable set
  — a bound on *acceptance*, not on execution.
* **Partitioned scheduling** in `HardRt`, no work stealing, which is what makes
  the admission test valid at all.
* **Bounded critical sections on the RT path**, by construction rather than by
  measurement.
* **Preemptible kernel**, so a long section delays rather than blocks --
  **except for the tasks that wait for it.** A task waiting for a kernel
  spin lock waits with its preemption count raised, so a long hold stops
  every processor whose task waits for that lock, and those processors make
  no scheduling decision and wake none of their sleepers until it is let go
  (F-95, open; §2.2y).
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
gives up and stops the machine (FX-0001 to FX-0003) if one never answers, and
waits for one that answers late.
That bound is a liveness diagnosis, not a safety property: waiting longer never
frees memory early, so its one job is to report a processor that will never
answer without ever calling a live one stuck.

Until 2026-09-26 it was one second of wall-clock time, and that measured the
host rather than the guest. Under QEMU's coverage plugin, which runs every
translated block through one process-wide lock, `test-compositor` stopped on
FX-0001 with nothing stuck. The bound is now two conditions together: the
wall-clock floor it always had, and a count of the waiter's own polls, which
slows exactly as the machine does and stands still while the waiter is not
running. A processor made to stop answering was then found in 1.8 s under
KVM, 5.1 s under `tcg` and 32 s under the plugin.

The residual was a host that starves one virtual processor while it runs the
waiter, and on 2026-10-07 it was measured (stage 20 S-1, FX-0001): with one
vCPU thread of QEMU made `SCHED_IDLE` beside three busy loops on one host core
of nazuna, a diagnostic kernel that waited on past the bound counted 21 waits
of a `test-boot --accel kvm --smp 4` and 236 of a `test-selfhost --accel kvm
--smp 8` past it, and every one was answered, after 1.0 to 3.6 s; `main`
stopped the same starved build on FX-0001 38 s into the starvation. So each
wait now has two bounds, each a wall-clock floor and a count of polls. Past
the **late** bound -- the old bound, 1 s for a shootdown and 5 s for a grace
period -- the answer is waited for and counted late (`L.smp.33`, the `late`
line), and reported once the shootdown turn is given back, a few lines a boot
and then a summary each time the count doubles (`L.smp.34`). Past the **stuck** bound,
10 s and 167,772,160 polls (about 5.5 s of polls under KVM, so the wall clock
governs there; about 51 s under `tcg`; about 5 min under the plugin), the
machine stops as before (`L.smp.17`); the turn's bound is four times that,
40 s (`L.smp.18`, not yet shown by a control). The consultant's verdict is
ledger line 455 of 2026-10-07.

What the late bound costs is availability, and it is a cost the item now
pays rather than a stop: while a processor answers late, its waiter's
processor runs nothing else, since the shootdown turn is a `SpinLock` and
disables preemption, and every other processor needing a shootdown queues in
`take_turn` for up to the 40 s (ASR-8, AoU-4). Nothing is freed and no
permission relied on before every processor has answered, as before. This
does not make the item tolerate host starvation in general: the same
starvation stopped two boot self-checks whose waits are wall-clock bounds
(FX-1011, FX-0882) and ran one boot past its 120 s timeout, each with a
`docs/BACKLOG.md` row.

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
return, run 500 times against a call's data, in the dev profile (then with
rustc's crate-local ThinLTO, which the dev profile has not had since
2026-10-15) in QEMU on the development host.

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
dev profile (with crate-local ThinLTO, as above), the two worst chains the
limits allow: 6,554 filters of one instruction (most runs of the interpreter, each with its loop and `Arc`
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

**`TSYNC` (S5, 2026-10-14).** A filter installed with
`SECCOMP_FILTER_FLAG_TSYNC` reaches every thread of the process in rounds
(`docs/SECCOMP.md` §3.7); its costs, measured by the `seccomp` boot line on
816efdf58 (`gate.sh` tags `s13fs-x86-4`, `s13fs-a64-4`, `s13fs-a32-4`):

* *The test, under no lock.* Step 2 tests each other thread's chain against
  the caller's by walking the caller's chain: up to 6,554 comparisons a
  thread, so threads × 6,554 a round, preemptible and paid by the calling
  thread alone. One thread against the longest chain costs 7,875 ns on
  x86-64 under KVM, 46,366 ns on AArch64 under TCG and 45,285 ns on
  ARMv7-A under TCG.
* *The holds.* A round takes the process's thread-list lock twice. Step 1
  costs, per thread, the list's `Weak::upgrade`, its `is_gone` test and a
  push into room made before (all in `with_live_threads`), then one thread
  lock and a reference taken; step 3, per
  thread, one thread lock, a pointer comparison or a binary search of the
  tested heads (at most 15 steps for 32,768 of them) and one chain swap.
  Nothing is allocated or freed under either. The boot check times each
  hold among 16 running threads, from the start of the closure
  `with_live_threads` runs under the lock to its end, so the figures leave
  out the listing above and are lower bounds of each hold: the longest of 2 holds was 1,400 ns on
  x86-64 under KVM, 892,800 ns on AArch64 under TCG and 679,136 ns on
  ARMv7-A under TCG (the whole call 14.2 us, 25.4 ms and 29.5 ms). Per
  thread that is 88 ns, 55.8 us and 42.4 us; for `PID_MAX`, 32,768
  threads, 2.9 ms, 1.83 s and 1.39 s a hold; for 512 threads (no reference
  `pids.max` is fixed for a user's session; 512 is the `TasksMax=` of
  `docs/INIT.md`'s getty unit), 45 us, 28.6 ms and 21.7 ms. The emulated
  figures carry TCG's cost and the gate host's load; no figure on Arm
  hardware exists.
* *Who waits.* The lock is a `PreemptSpinLock`: preemption is off on the
  holder's processor and interrupts stay open. It is waited on by the
  process's own `clone(CLONE_THREAD)`, its threads' `attach` and
  `set_strict` (each an O(1) hold of the same lock), a second `TSYNC`, and
  every caller of `Process::threads`. Among those, another process reaches
  it when it sends the process a signal (`signal_taker`), reads
  `/proc/<pid>/task` or a cgroup's `cgroup.threads` (`thread_ids`), or names one
  of its threads through a pid namespace (`pidns`). Another process can
  therefore be made to spin, preemption off on its processor, for one hold:
  the target's thread count × the per-thread figure above.
* *Rounds.* A round is given up only when a thread changed between the two
  looks; one given up for a thread's own filter or strict mode is followed
  by one that answers that thread's id, and the one change that gives a
  round up and lets the next go on is another thread's completed `TSYNC`,
  which lengthens a chain capped at 6,554 filters. So a call makes at most
  6,554 rounds given up for `TSYNC`s, plus one for each thread that changed
  itself, before it answers.

The stated worst case of one hold passes 10 ms: for 512 threads on the
emulated Arm boots, and for `PID_MAX` threads on both. That is **F-72**
(open, `FINDINGS.md` §E), with a BACKLOG row: bound the hold by the
process's thread count, or confirm in batches that drop the lock between
them.
**Syscall User Dispatch (`docs/SECCOMP.md` §13, ledger line 2323).** A
thread that asks for dispatch is watched as a filtered one is, and a kernel
where no thread is watched makes the same loads at a call as before (the
mask and the count are shared, UD9). For a dispatching thread a call adds:
one leaf lock, `Thread::dispatch` (a `crate::sync::SpinLock`, preemption
off, nothing taken inside it, never held across a fault or a signal),
held for a 32-byte copy; and, for a call from outside its range, a one-byte
`copy_from_user` of the selector. That read is the look's new worst case:
**one user page fault**, which may allocate a frame or wait for a file
page's I/O, taken with interrupts open and no lock held, before
`call_entered` (condition 4). It is on the path only of a thread that asked
for dispatch, never on the path every call of every program takes, which
`trap::SyscallFilter`'s contract binds. The `dispatch` boot line measures
the in-range, ALLOW and dispatched costs in ns.

### 2.2c What the cgroup controllers add to the time claim (2026-10-01)

Five costs the controllers put on paths the claim covers, stated with their
bounds, which are *not* constants, so AoU-4 and ASR-8 do not rest on them:

* `charge_cpu` (every tick) and `throttled_until` / `runtime_left` (the arm of
  the next timer) walk the job's slot and every slot above it, under the
  processor's run-queue lock with interrupts off. The cost is the job tree's
  depth, and nothing bounds that but what a program makes (cgroupfs has no
  `cgroup.max.depth`). A deployment that relies on a bound on the scheduler's
  tick should bound the depth of the cgroups it runs programs in.
  `cpu.max.burst` (2026-10-26, L.object.184) adds a constant per slot to each
  of the three walks (one load of what was carried, in `allowance`) and a
  constant to the one caller that starts a period (`carry_bound` and
  `carry`); the depth bound is unchanged. Between `carry_bound`'s store and
  `carry`'s, a charger compares the new period's use with the most that can be
  carried, so the window lets at most one charge's overrun through -- the same
  bound as the overshoot of any period's last charge -- and never throttles a
  task short of its runtime.
* `io.weight` and `io.latency` (2026-10-26, load ring, `fs::blkio`) put a
  disk's requests in a fair queue while any cgroup sets either, one request in
  flight at a time. With neither set, a request pays one load (`SHAPED`).
  While set:
  * under `SHAPES` then `DEFAULTS` (the module's lock order; `TABLE` is never
    held with either, nor a disk's fair lock with any), a request looks up its
    weight and target at each level of its job's chain: depth x log(shapes);
    every `io.weight` or `io.latency` write and every job's end (`forget`)
    scans every shape under `SHAPES` (`reshape`);
  * under the disk's fair lock, choosing the next request is O(waiting), and
    each completion's `wake_all` has every waiter choose again: O(waiting^2)
    a completion. The waiting list is kernel heap charged to no job, one
    entry per task waiting on the disk, never shrunk;
  * the wait is uninterruptible and its length is bounded only by the weights:
    about 10^4 times a sibling's share at each level against a busy sibling,
    more when nested, and up to 2 ms more while the disk is held for another
    group, or behind up to seven urgent requests; a kill is acted on once the
    request has been served. A hold set after a waiter looked wakes it to take
    the hold's end as its deadline.
* `object::oom::throttle` runs
* `object::oom::throttle` runs after a successful fault and after a read of a
  cached file, and does nothing when the job and its ancestors are under their
  `memory.high`: one walk up. When one is over, it reclaims (a scan of every
  file object in the machine, `user::cache::reclaim`), lists the live processes
  to wake `memory.events`' pollers (`process::live`, an allocation) and may
  sleep a millisecond. That is a bounded pause but not a bounded cost, and it
  can fall under a lock the reader holds: the read of a cached file is made
  under its filesystem's lock, which on a shared writable volume is mount-wide.
* `settle_frozen` lists the live processes and walks the job's ancestors on
  every park, leave and move of a frozen process: O(processes x depth).
* A throttled task sleeps to its period's end, woken before then only by a
  wake of the task itself, which every kill, signal and stop is posted with
  (since F-78, 2026-10-16; until then it slept in slices of a millisecond,
  asking at each whether the way back to user mode had anything to do).
* `sched::charge_running` is reachable at will by any program, not only
  through a read of `cpu.stat`: `CLOCK_PROCESS_CPUTIME_ID`, `times()`,
  `getrusage(RUSAGE_SELF)` and a read of `/proc/<pid>/stat` or
  `/proc/<pid>/task/<tid>/stat` call it (since ef-sys-utime, consultant ledger
  2652), so that another thread's time is current. It masks interrupts on the
  caller for the walk of every run queue, takes each queue's lock in turn --
  lock traffic on every processor, the processor of a channel round trip
  included -- and settles each queue's Pending charge up the job tree. Its
  cost is processors x (one lock + the job tree's depth), unbounded in the
  depth as the first bullet says.

None of the first four is on the path of a task in a job with none of the limits set,
which pays one load (`bandwidth_in_use`) and the `over_high` walk.

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
p50 2,287 ns against 2,427 ns without step 3, the median of five rounds,
ratio 0.942 (`~/.local/share/ferrix/logs/queue/po7-ipcB-s3r-bench-1.log` on
nazuna, on step 2f; OPAQUE-KERNEL.md §9.9).

**The realtime tier's walk (step 4b, `desktop` alone).** `FifoRr`'s
queue is a balanced tree: `pick`, an enqueue and the removal of the running
entity are O(log n) or better in the processor's real-time tasks; `remove`
of a waiting entity by identifier walks the tree, O(n), under the queue's
spin lock. Every removal is routed by the task's tier
(`StackTiers::remove_in`, the review's line 30, B3), so the removal of a
fair task, a steal's among them, never walks this tree, and the walk serves
one caller alone: a change of policy on a queued real-time task that does
not run (L.sched.112), at the call or at its processor's next decision. A
`present` build has no such tier, and its paths are as below.

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

Since F-69 (L.sched.71, L.sched.72), one more span decides nothing until
it ends: a wait's last look. A wait raises the preemption count before it
sets its task blocked and lowers it once the look is over, so that no
switch can take the task off its run queue inside the look. An interrupt
that asks for a decision inside the span is still taken; only the
decision waits, until the span ends:
* on a look that finds its condition, the decision is made at the
  lowering;
* on a look that blocks, the span ends inside the switch's own interrupt
  mask (`block_ending_hold`, F-69's cheaper hold), and the switch is the
  decision. A request left set reaches this processor's next way out
  (`call_left` at count zero, or an interrupt's exit), as it did across
  `block` before F-69, and is decided there again.

L.sched.7's deferral is unchanged. Since F-77 (2026-10-16) the
single-queue wait raises the count before it lists the task, so the span
also holds the listing: the waiter list's lock, one fallible push and the
deadline's store, with `set_state` made under that lock. The list's lock
masks interrupts, and its masked span now holds `set_state` too: one swap
and `quota::adjust`'s lock-free walk up the job tree, bounded by the tree's
depth, as under the run-queue lock where `wake_at_home` makes it.
`wait_on_any` raises it before `BLOCKED` and its listings, which the span
now holds, one lock and one push a queue.

The span holds:
* the listing, under the list's lock, for the single-queue wait and for
  `wait_on_any`;
* `set_state`;
* the fence, made only for a trusting wait;
* one call of the wait's `ready`;
* for the receive half's park, `Endpoint::park`'s inbox lock and one load
  of the task's `END` bit.

Its worst case is `wait_on_any`'s look for `poll`, `select` and
`epoll_wait`, which asks each watched file once. That is one `poll` per
watched descriptor, each under the file's own spin locks, which already
held the count up for most of the look before this change. The bound is
linear in the number of descriptors a program watches, as that look
always was.

The one `poll` with more in it is the console's. Its line discipline can
echo to the UART synchronously and send a signal to the foreground group
(`fs/terminal.rs` `pump`). Inside the span `console::emit` takes its
synchronous path (`may_block` is false), where before it could wait for
room in the ring, a nested wait inside a wait's look.

No `ready` may block, and the audit of every `ready` and every reachable
`poll` found none that takes a `SleepLock`, waits for memory, touches user
memory or nests a wait (os07-stall's audit of 2026-10-08). A `ready` that
parked would stop the machine with FX-0503.

So the deferral stays inside L.sched.7's bound, delayed by at most one
look.

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

**The lent framebuffer's read-back (`ferrix.efifb` only).** The kernel's
own panic window is write-combining from its first map on x86-64
(L.x86_64.176), so lending the firmware framebuffer
(`discovery::firmware`) changes no type: `window_combining` reads the
window's leaves back, one table walk per page, at most
`FRAMEBUFFER_WINDOW_SIZE` / 4 KiB of them, under no lock -- safe only
because the kernel's upper-half intermediate tables are never freed, so a
walk cannot meet a table given back under it -- and no shootdown follows. It runs once per boot, at stage 10, before
any program, and only under the option; the reference configuration never
reaches it (stage 9's check calls it on every x86-64 boot, twice). Then one
hold, kept for good, adds one entry here. At the window's first map, on
every x86-64 boot with a framebuffer, `flush_window` flushes each cache
line of the window once (8 MiB at 1920x1080 is 131,072 lines), before any
other processor runs.

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

**The Intel integrated GPU's budget** (`L.object.194`-`196`; the
consultant's K3 (3), ledger 2352). Counted at stage 10 with the ceiling,
from the frames the allocator manages, `U`, and the watermark, `W`
(`mm::watermark`, `U / 128` held between 256 and 8192 frames):

    B = min(INTEL_GPU_CAP_PAGES, (U - W) / 4),   INTEL_GPU_CAP_PAGES = 4 GiB / 4 KiB

so the pages its pins hold -- live, quarantined and kept, at most `2B` by
the rules above -- are at most `(U - W) / 2`, half the RAM above the
watermark, and at most 8 GiB. On the N150 box's 16 GB (about 4 M frames
managed, so `W` = 8192) `B` comes to about 3.8 GiB, just under the cap,
by what firmware reserves; on the x86-64 test boot's QEMU guest, whose
allocator manages 110,665 frames with `W` = 864, it is 27,450 pages.
It is set on the GPU's domain under this lock as the domain is made, before
any pin can be counted against another budget, and it also bounds what
`device_set_limit` may set: nothing of the ceiling is held for it. A pin past
`B` is `LIMIT_REACHED` (`ENOSPC`), one past `2B` `QUARANTINE_FULL`
(`EDQUOT`), each with nothing pinned or counted: the same test and store
under the lock as any device's, with no walk.

**Lock order: a node's domain lock, then this one** (ledger 2356 (1)).
`DeviceNode::domain` gives the Intel GPU its budget while it holds the
node's `domain` lock, so that no pin can be counted against another budget
before it: that takes this lock inside the node's. Nothing takes them the
other way. None of the twelve `QUARANTINE.lock()` sites (eleven in
`object/pin.rs`, one in its check) calls `DeviceNode::domain` or
`domain_made`, or takes any lock, while it holds this one:
`budget_of` and `quarantine_release` read `domain_made` before they lock,
and `release` unpins and frees after it drops the lock. This lock stays a
leaf below the node's.

**What its pins are charged to** (ledger 2356, ruling 3). A pin holds a
VMO's frames. Each frame is charged to the job that committed it, at first
touch (`mm::allocate_user_frame`), and the pin does not move the charge. The
driver's own GEM objects are therefore charged to the driver's job. A
dmabuf the driver imports and pins, whose VMO another client or driver
committed, stays charged to that job, and its pages still count against the
GPU's `B` and `2B`. K7 (userptr, client pages pinned by the driver)
reopens this.

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

A fault take (`iommu::vtd::Unit::take_fault`, under the unit's `taken`
lock, an `IrqSpinLock`) reads the unit's fault recording registers from the
one `FSTS.FRI` names round all of them (`L.iommu.64`): at most
`CAP.NFR` + 1 = 256 records, four register reads each, with interrupts
masked on that processor; the N150's units and QEMU's have one record, so
the hold is what it was. The end-of-boot audit's 256 takes a unit bound how
often it is entered. Protected memory regions, where a unit has them, are
turned off once at bring-up with a wait bounded by the unit's patience,
100 ms (`L.iommu.68`).

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
ring, outside the item; the item gains only its call numbers and their
`SERVED` rows (eleven, the last K5's `chardev_dmabuf_fence`). Its bounds are written here because the consultant made them
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

**A dmabuf's fence slot (K5, `docs/INTEL-GPU.md` §5; the consultant's K5
(4), ledger 2352).** Each dmabuf has one slot, `Dmabuf::fence`: its
serving driver's latest queued GPU access of any kind to the buffer, reads
included, so the driver sets it for reads as well as writes (ledger 2360).
It is an
`Option<Arc<SyncFile>>` under a spin lock of its own. It is a leaf. It is
held for one `replace` (`chardev_dmabuf_fence`) or one `clone` (an export),
a few loads and stores, and no other lock is taken, nothing allocated and
nothing waited for under it. The fence a `replace` gives up is dropped after
the lock is released, because a fence's drop takes its control's `syncs`
table. Its memory is one pointer a dmabuf. The fence it holds is one of the
control's own, made by `sync::make` and counted against `MAX_SYNC_FILES` and
`MAX_WATCHED` as any fence, and freed with the last of the slot and the
descriptors that hold it. An export allocates one descriptor, and an empty
slot's export also makes one signalled fence (two fallible allocations,
`ENOMEM`). An import allocates a request as an ioctl does, and its client
waits for the driver's answer as for an ioctl (above). No wait on the fence
itself is made by the kernel, by the importing program's call, or by any
other program.

### 2.2l A tmpfs inode's locks (`ferrix_vfs::tmpfs::Node`, load ring, F-70)

Two locks per inode since 2026-10-10, taken in one order: a regular file's
`FileIo::io`, a `SleepLock` over the scheduler's parker (`SchedParker`, which
the kernel's `VmoStorage::parker` hands every tmpfs it makes), then the inode's
`state`, the crate's preemption-holding spin lock, then the VMO's own locks
under it. `io` is never taken under `state` or under any spin lock -- the
`SleepLock`'s `may_park` stops a debug kernel that tries -- and nothing a VMO
or a pager does re-enters a tmpfs inode's `state`. `state` covers words and
maps only: the length, seals, times, owner and a directory's names; what
`stat` reads of the store under it, the committed page count, is a VMO's
spin-locked count, never a sleep. A read's fill, a write's frames, a job's
`memory.high` throttle and a truncation's decommit and TLB shootdown all run
under `io` alone, so a holder of `state` never blocks (FX-0503 stops the
machine if one does) and is never switched out, and a waiter for it waits
for a few map operations. The callers of a tmpfs file's `read_at`,
`write_at` and `set_len` -- the system calls, the exec loader, `sendfile`
and `splice`, memfd and System V shared memory -- hold no spin lock when
they call, as they already had to for a file on btrfs.

Before, `state` was a plain ticket lock held across all of that work. A
reader throttled at `memory.high` slept holding it (F-70), and with Steam's
eighty staging threads listing and `stat`-ing one directory the holders and
waiters switched out mid-turn kept all four processors spinning in
`metadata` for as long as Steam ran (docs/STEAM.md §7; the same convoy
875b66f3b took out of the rest of `ferrix-vfs` on 2026-10-03).

**The dmabufs (N3b, ledgers 316 and 726).** A control's `dmabufs` table
holds one entry per live dmabuf it made, at most `MAX_DMABUFS`, 4096; the
4097th install is `LIMIT_REACHED`, and stage 10's check makes 4096 through
the product's `find_or_make` and requires the refusal. Making one reserves
its table entry and its release's queue slot (`hold_release`) before the
object exists, so its drop allocates nothing. Under the table's spin lock:
an install's scan for its cookie and the gone test (at most 4096 entries),
a drop's `retain` (at most 4096), and the self-check's `alive` count (at
most 4096); nothing is copied, waited for or mapped under it, and the one
lock taken beneath it is the control's `state` (an install's
`hold_release`, a drop queueing its release before it lets the table go);
`state` is never held while the table's lock is taken. `finish` takes the table's lock
once, empty-handed, after storing gone. The render numbers a chardev
driver is lent sit in `RENDERS` (one per control, so at most one per
device) and the render core's `LENT` (the same count), each walked under
its own lock at publish, unpublish and `/dev/dri`'s listing; `LENT` is
taken under `RENDERS` and never the other way. A dmabuf's install and
resolve each count as a copy of their request, so a program's call does
not return while one runs (N4).

**The fences (N3b sync, ledgers 647 and 725).** A control's `syncs` table
holds one entry per unsignalled fence it made, at most `MAX_SYNC_FILES`,
4096 (`LIMIT_REACHED` past it; stage 10 makes 4096 through the product's
`make` and requires the refusal). The `syncfiles` thread's `WATCHED` list
holds each fence until it is signalled or dropped, at most `MAX_WATCHED`,
16384, across all controls: an install that would pass it first prunes the
list of signalled and dropped fences, then is refused `LIMIT_REACHED`.
Under `WATCHED`'s spin lock: `watch`'s prune and push (at most 16384, one
push into room reserved under the same lock), and a look of the thread
(`pass`), which walks the list once, at most 16384 entries, keeping the
unsignalled ones, moving at most 32 fences past their deadline into a
fixed array, and allocates nothing; their signals run after the lock is
let go, and a full array has the thread look again at once. A fence whose
last holder went during the walk drops under `WATCHED`, taking its
control's `syncs` lock: `syncs` is never held while `WATCHED` is taken.
Under `syncs`: `make`'s scan (at most 4096) and push into reserved room,
the gone test, a signal's or the thread's take (at most 4096), a drop's
`retain` (at most 4096). `control_gone` takes the table out under the lock
and signals each fence after it, with no allocation. A fence's own lock
guards a status word. A program waits on a fence through `poll` with the
deadline the fence carries, at most `SYNC_DEADLINE_MAX_MS`.

### 2.2m The pid table's lock (`object::process::TABLE`)

A spin lock over the map from number to process. `allocate`, `name`,
`release`, `release_naming` and `find` are one lookup, insert or removal
in the map. Three things walk it whole, each at most `PID_MAX` (32768)
entries: `numbers_naming`; the gathering of `live` and `listed`, whose
list of strong references is collected fallibly under the lock (an
`AllocError` refuses the whole listing) and whose references are dropped
after the lock is let go; and, since 2026-10-12, `unlist`, which a reap
runs once for the reaped process, clearing the `listed` flag of every
entry naming it and allocating nothing (`L.object.181`). Nothing is
waited for or locked under it.

### 2.2n Reclaim of a writable mount and the cache totals (2026-10-15)

Bounds of the locks the writable mount's reclaim adds (`L.object.188-190`):

* `Vmo::commit_and_write` and `Vmo::zero_from` hold an object's pages lock
  for one lookup, at most one frame allocation and zeroing, and one copy of
  at most a page; `Vmo::take_clean_range` adds two atomic loads to
  `take_range` under the same lock.
* `user::vmo`'s `RECLAIM_HOOK` is a leaf spin lock, held to copy one
  `Option` out, once per object chunk a reclaim takes, never while the hook
  runs. Only stage 12's check arms it, and the boot stops (FX-0908) if it is
  still armed after.
* The btrfs glue's `Source::reclaim_clean` (load) runs a `retain` over the
  pages picked (at most `CHUNK`) with `Still::holds`, a linear scan of the
  file's busy ranges, under its `dirty` and then `still` spin locks: O(picked
  x writers of that file at once), each step bounded. It calls the take with
  neither held.
* `user::cache`'s `HAND` (2026-10-15) is a leaf spin lock held to copy or
  replace one `Option` of a `Weak` and a page index, twice a reclaim; the
  hand's object is found in the list of live objects with no lock held,
  O(objects), the cost the list itself already has. The `Weak` it replaces
  is dropped after the lock.
* `user::cache::totals` takes each live cached object's pages lock once, for
  its page count: O(objects) per read of `/proc/meminfo`, nothing allocated
  but the list of live objects.
* `Shared::give_way` and `Node::hold_still` (load) wait with no lock held:
  parked on the mount's `Parking` when it has one, and spinning without one
  (no memory for it at mount), which then needs the waiting task to be
  preempted in task context for the writeback or reclaim it waits for to run.

### 2.2o The pstore region's write-back and its boot check (`console::pstore`, PM3)

`docs/POSTMORTEM.md` §3. The panic path's write-back (`L.console.44`) runs
once, in the first report, after `console::drain` and before the screen,
with the other processors already asked to stop; it takes no lock and
allocates nothing. Its time is bounded by the hardware:

* **x86-64**: `clflush` by address over the region alone and one `mfence`
  (`clean_for_walker`, the IOMMU's own): 16 448 at the 64-byte line
  `CPUID` reports. Not `wbinvd`, which §3.6 of the design had: it takes no
  address, its time is every cache's size, and it is the one instruction
  here a hypervisor must emulate whole; `clflush` writes the line back from
  every cache in the coherence domain, which is the claim needed.
* **AArch64**: `dc cvac` by address over the region alone, `(4 KiB + 1
  MiB) / line` instructions and one `dsb sy`: 16 448 at 64-byte lines, the
  line size `CTR_EL0` reports on the reference machines.
* **ARMv7-A**: `DCCMVAC` over the region alone: 32 896 at the Cortex-A7's
  32-byte lines (the DK1), 16 448 at 64.

The same write-back runs, through `console::pstore::stop` (`docs/POSTMORTEM.md`
§5), before the reset `ferrix.onexit=reset` asks for and before finish's
power-off, and in each of `reboot(2)`'s four stopping arms (`L.console.47`):
there in a system call after `sync_disks`, with interrupts on, the other
processors running and no lock held, once, on a path that ends the machine.
Each stop adds one atomic update of the stop word before its write-back
(`L.console.46`), a compare-and-exchange that only a stop racing another on
a second processor repeats, at most once per such stop; the first report adds
one store of the panic's word as it begins.
Under `ferrix.panic=reset` (H.FAIL.5) the panic path's bound gains 10 s on
the counter, then `arch::reset`'s bounded tries.
The mirror itself (`L.console.42`) is the log's own lock-free record, once
more per byte: a store and a compare-and-exchange on the region's count.

The boot check that no frame of the region is free (`L.mm.75`) takes the
frame allocator's lock once a frame, 257 times for the region, once a boot
after the reclaim, on the boot processor.

### 2.2p The firmware's SMBIOS tables (2026-10-15)

* **Memory.** The loader's copy is the entry point padded to 32 bytes and
  the table, at most 1 MiB (`ferrix_smbios::TABLE_MAX`), rounded up to
  pages, in `MemKind::Smbios`, resident for the life of the system. The
  kernel copies nothing: it borrows it (`discovery::smbios`).
* **Time.** The kernel checks and walks the table once at boot, O(length),
  under no lock; the result is a write-once `Once`.
* **Reads.** A sysfs read renders one field (at most a page) or returns
  the entry point or the walked table, bounded by the copy's length; it
  takes no lock but the `Once`'s completed check.

### 2.2q A wait queue's marks (`sched::wait`, epoll's ready list, 2026-10-17)

A wait queue's wake tells every mark hooked on it (`L.sched.74`), and that
walk runs with interrupts masked under the queue's hook lock, from whatever
woke the queue, an interrupt handler included. Its bound:

* **Per mark**, an atomic swap and, for a mark not yet listed, the push of
  its number onto its ready list under that list's leaf lock (room reserved
  in advance, so nothing allocates); the list's queue is woken at most once
  between two takes of the list (`L.sched.76`), after the leaf lock is let
  go.
* **Marks on one queue** are bounded by the registrations charged to the
  jobs that made them: a queue many jobs share (the net core's progress
  queue, every inet registration of a namespace) carries all of theirs
  (`VULNERABILITY-ANALYSIS.md`, T.EXHAUST path 11).
* **Nesting**: a list's queue is hooked only by the marks of the epoll sets
  that hold its set, at most `MAX_NESTS` deep, so the chain of hook locks is
  at most `MAX_NESTS + 1` long, and acyclic (`fs::epoll`'s `NESTING` lock
  makes the depth check and the add one step).
* **Measured** (x86_64 under KVM on nazuna, `hookwalk`, a static program run
  as init, 800 registrations of dups of one eventfd): a write whose wake
  finds 800 marks not yet listed costs 13.1 us against 1.0 us with none, a
  12 us walk, 15 ns a mark; with the 800 already listed, 6.9 us; with the
  set nested once in another, 13.2 us. Logs in the landing message.
* **Charged** per registration: the file it pins, its mark, four places in
  each of up to 8 queues' mark lists and one boxed list header for each
  (`fs::epoll::HOOK_ROOM`, an upper bound).
* **Unhook** searches the queue's mark list linearly, under its hook lock,
  so removing every registration of a queue with n marks is O(n^2); the
  list gives half its room back once a quarter full, and is freed with its
  last mark.

Lock order: a queue's hook lock, then a mark's list lock (a leaf, let go),
then the list queue's waiter lock, the run queue's lock in `wake_with`, and
that queue's hook lock, and so on up the nesting. An epoll set's state lock
is taken before a queue's hook and waiter locks (hooking, unhooking) and
before its list's lock (taking), never after either; `NESTING`, a sleeping
lock, before a set's state lock. Nothing takes a hook lock under a waiter
lock or a run queue's lock.

### 2.2r Kernel time and switch counts (`sched::vtime`, 2026-10-18)

A task's kernel time is counted at every entry from user mode and every
return (`L.sched.80`-`82`, `docs/THREAD-TIMES.md`).

* **Memory.** One static array, `vtime::SLOTS`: 256 processors (the bound
  `IN_CALL` has) of one 64-byte line each, 16 KiB, never allocated. Each
  task grows by 24 bytes: its kernel ticks and its voluntary and
  involuntary switch counts. Nothing allocates on any path below.
* **Time.** On every system call entry and every trap taken from user
  mode, with interrupts masked: one counter read (`RDTSC`, `CNTVCT`) and a
  few loads and stores of the processor's own line, no lock, no walk; the
  same at a return that closes a stint an entry opened, and none at one a
  decision opened; none at the entry of x86-64's three blocking native
  calls; at every decision, loads and stores with the decision's own
  reading. A return with no stint open (the fast path's quiet exit) is a
  load and a branch. Under a hypervisor that traps `RDTSC`, each read is a
  VM exit; KVM on nazuna does not trap it. Measured cost on the round
  trip: `docs/THREAD-TIMES.md` §5.
* **Where the counter is the HPET**, nothing is read: every stamp is a
  load of the boot's one flag and a branch.

### 2.2s The check hooks' locks (`arch::speculation::LEAVE_HOOK`, `FORGET_HOOK`)

Two leaf spin locks, each over an `Option<&'static CheckHook>` that only
stage 9 sets (`docs/OPAQUE-KERNEL.md` §9.7 condition 11). Each is held only
to store, clear or copy that `Option`: `leaving_domain`, and
`run_forget_hook` for `forget_root`, copy it out and let the lock go before
the hook runs, so no hook runs under either, and nothing else is locked or
waited for under them. `LEAVE_HOOK` is taken once per leave of a
speculation domain, from a context that may block. `FORGET_HOOK` is taken
by production code on every successful compare-exchange in `forget_root`,
that is once for each processor whose last root was the one being
forgotten, in the contexts `prepare_user_root` is called from on all three
architectures (a new address space's root, before it is ever installed),
which may hold other locks or have interrupts masked; as a leaf, it adds
no ordering to them. Since F-64's fix (2026-10-14).

### 2.2t The view of a task that is not running (`sched::inspect`, 2026-10-21)

What `/proc/<pid>/task/<tid>/syscall`, `wchan` and `stack` read
(`docs/DEBUG.md` §1, `L.sched.86`-`89`; the consultant's design review,
ledger 2831, conditions 1 and 2). Nothing runs on any path the task takes:
the view only reads, at a `/proc` open.

* **Memory.** None allocated in the item: a sample of five words on the
  stack, the chain into the caller's array of `backtrace::MAX_FRAMES` (24)
  words.
* **The run queue's lock.** `inspect::sample` takes the lock of the queue
  that owns the task, with interrupts masked, as `set_weight` takes it,
  and holds it for a pointer comparison and five loads: the state, the
  saved stack pointer, `in_call`, the switch count, and the stack's bounds.
  It retries only when the task moved between its processor's read and the
  lock, as `wake` does. Nothing else is taken while it is held, and it is
  let go before any word of the stack is read.
* **The page tables' lock.** Each word is read by `mm::read_kernel_word`,
  which takes `TABLES` (interrupts masked) for one walk of at most the
  paging depth -- four levels on the 64-bit pair, three on ARMv7-A -- one
  RAM-run test (`direct_map_ram`, at most `RAM_RUNS` comparisons) and one
  word's load, then lets it go. An open reads at most 2 x `MAX_FRAMES` + 2
  words of the chain, or the entry frame's 9 fields, each taking the lock
  on its own. The two locks are never held together.
* **What a reader can cost others.** A reader allowed to open the files
  can take `TABLES` about 50 times per open, back to back, each for one
  walk; a loop of opens contends with `vmap` and every other kernel mapping
  change on the lock, as a loop over `/proc/<pid>/maps` contends with a
  space's lock. `stack` is root's alone; `syscall` the same user's.
### 2.2u A woken task placed on an idle processor (`sched::place_on_idle`, 2026-10-20)

`L.sched.85`; the certification consultant's design review, ledger 2658.

* **Memory.** None allocated: the task moves into the destination's queue
  in its own run slot (`CpuQueue::insert`), as a steal moves it. Two
  counters a queue (`placed_in`, `placed_out`).
* **Time, a wake** (`wake_at_home`, the home queue's lock held, interrupts
  masked): only when the home processor runs a task, the wake is made
  from another processor, not by the task home runs, and the task is not pinned.
  Then one load of `IDLE`, a walk of at most the online processors from the
  one after home, and for each idle one the task may run on a `try_lock` of
  its queue, never a wait, and a read of two words under it. With a queue
  taken, one `insert` (the run queue's EEVDF placement, as a steal's). That
  is O(processors), no allocation, no wait.
* **Time, a sleeper** (`choose_next`, after `wake_sleepers`, the processor's
  own queue lock held, interrupts masked): the same for each sleeper whose
  time came while this processor ran a task, so O(sleepers due x
  processors). Each processor a sleeper was placed on is told with `kick`
  under this lock. `kick` waits for no answer from the target. x86-64 in
  x2APIC mode: one `WRMSR`, nothing waited on. In xAPIC mode the local
  APIC's acceptance is polled, bounded at 1,000,000 reads (`apic::issue`),
  as every kick already is; the target's reply is never waited for.
  AArch64 and ARMv7-A: one SGI to the other processors (`send_ipi_to` there
  is the broadcast), a `DSB` and a register write, nothing waited on.
* **Lock order.** A queue's lock is now taken while another queue's lock is
  held in a second place: `place_on_idle` takes the destination's with
  `try_lock` while the home queue's is held, in either numeric order.
  `steal_from` keeps the lowest-numbered first and waits. The new nesting
  cannot deadlock, because the second lock is only tried: a processor that
  holds it is never waited on, so no cycle of waits can include this step.
  A failed try costs that one candidate and nothing else. No check asserts
  the steal's order for two queue locks.

### 2.2v A woken task pulled onto its waker's processor (`sched::pull_onto_waker`, 2026-10-22)

`L.sched.90` and `L.sched.91`; the certification consultant's design review,
ledger 3062; design in `docs/SCHED-WAKE-AFFINE.md`.

* **Memory.** None allocated. Three words a task (`last_wakee`,
  `wakee_flips`, `flips_halved_at`), written only by the task itself while
  it runs; one counter a processor (`PULLED`). A pulled task moves into the
  waker's queue in its own run slot (`CpuQueue::insert`).
* **Time, a wake from a call** (`wake`, interrupts masked for the
  decision): at most one clock read, three loads and three stores to the
  waker's own words (`record_wakee`, Rule B: no lock; the clock is read
  only when the wakee changed or the flip count is above zero), the domain and
  affinity tests, one load of `IDLE` and a walk of its set bits (at most
  the online processors). When it is tried, the pull is `wake_onto`'s: the
  two run-queue locks, the waker's and the home's, taken lower processor
  first as `steal_from` takes them and as Wake::Sync already took them,
  each waited for; under them one clock read and at most three loads and
  three stores for the wakee's busy share (`Task::busy_share`), two load
  averages compared, `asleep_at_home`, one `insert` and `kick_after_wake`. O(1) under the locks, O(processors)
  before them. The same lock pair and order as the Sync wake on main, so
  no new nesting.
* **The per-task bound.** Placement decides which queue a task waits on,
  not how a queue serves it. The EEVDF bound of H.SCHED.2 is per queue and
  unchanged (`SAFETY-MANUAL.md` FM-8).

### 2.2za Scheduling domains made at run time (`sched::domains`, 2026-10-28)

`H.SCHED.21`-`23`, `L.sched.118`-`128`; the certification consultant's
design review, ledger line 32 (OK IF V1-V16); design in
`docs/SCHED-CLASSES.md` §9f, step 5a as built.

* **Memory.** None allocated on any scheduler path: a task taken off its
  queue for its domain waits in `MOVING` in its own run slot, as a dead one
  waits in `ZOMBIES`; one that cannot be filed is counted (`MOVING_LOST`,
  zero on a correct kernel, which the domain check requires). The switch
  and the admission allocate, in a task that may sleep and before anything
  changes: one vector of the tasks of the live processes walked
  (`domains::members_of`), refused as `NoMemory`.
* **Time, every path that places a task** (a spawn, a wake, a pull, a
  placement on an idle processor, a steal, the balance, the pick, the
  direct switch): one load of the processor's word, one of the task's, one
  of `LIVE` (`domains::admits`), under the lock the path already holds. A
  steal and the balance walk their own domain's processors only; the
  pull's factor is a count kept with the domain (`domains::count`).
* **Time, a decision on a leaving processor** (`choose_next`, its own
  queue's lock held, interrupts masked): `rehome_waiting` walks the fair
  tree and the tier above it once for each task it takes off, so
  O(waiting^2) under that lock, each task taken off filed in `MOVING` under
  its own lock (a leaf, taken after any queue lock and never before one).
  Only while a switch is under way; on any other decision, one load.
* **Time, the placement of a task taken off** (`domains::drain`, with no
  queue lock held, from `finish_switch` on any switch while one waits, from
  `enqueue`, a wake, and the switch's own wait): `SWITCH` is only tried,
  never waited for, so a switch away from a task never waits on a switch of
  domains; under it, for each task filed, `choose_cpu`'s walk of every
  queue's lock in turn and one lock of the target's queue. A task with no
  processor of its domain now is filed again and looked at once a call.
* **Time, the switch** (`domains::create_soft`, a task that may sleep): the
  processors' words written each under its queue's lock, then a wait,
  sleeping 1 ms between looks, bounded at 1 s (`PATIENCE_NANOS`), past which
  the processors are given back and the switch refused (V6). Its length is
  the kick's latency, the leaving queues' walk above, and the longest
  non-preemptible section a leaving processor is in (F-95's, a reference
  here and not a bound, C2). Dissolution is one store to `LIVE`, the
  processors' words after it, and no wait.
* **Lock order.** `SWITCH` (an interrupt-masking spin lock) before any
  queue's lock; `MOVING` after any. No queue lock is taken while `MOVING`
  is held, and `SWITCH` is only tried from a context that may hold none.

### 2.2w The power core (`arch::x86_64::powerctl`, 2026-10-22)

Present only under `ferrix.powerctl` on the N150 (`docs/POWERCTL.md`).
**Memory:** a claim allocates a channel pair and one task, both fallibly
(`NO_MEMORY` to the caller, nothing kept); nothing else. **Locks:**
`STATE`, a spin lock, is held for one call's validation, at most one write
of 0x601 (three reads, two writes of it, each a single `rdmsr`/`wrmsr`) and
one restore, all bounded; the ended claim's kernel end is dropped after it
is let go. No line is printed under it, nor inside the write's or a task
restore's masked section, at most 13 and 6 accesses to atomics and the
MSR. **Work:** the claim's
task wakes once a second and reads at most three MSRs; `power_set` and
`power_read` are the driver's own calls, charged to it, each a bounded
number of MSR accesses (`power_read` eleven). **Lines:** the kernel prints
one line per write taken, per restore and per latch; a write of the value in
force writes and prints nothing, and a raise is taken at most once a second.
**Time to act:** the backstop, the lease and a holder's death are seen at
the task's next look, within one second of the condition plus the boot
processor's scheduling latency, and acted on in that look; the lease itself
is 5 s. Nothing here bounds the time to a restore if the scheduler stops
running the task (V-16).

### 2.2x A record written to the kernel log (`console::write_record`, 2026-10-23)

A `/dev/kmsg` write (`fs::kmsg`, load ring) is one record of at most 1,024
bytes, refused above that. `console::write_record` writes it, and a newline
after it if it has none, under the console `PORT` lock as a kernel line is
written: queued for the transmit interrupt or polled, never waiting for
room, so the lock is held for at most 1,025 bytes' queueing or polling,
the same bound a kernel line of that length has. Recording it in the log
is `log::record`'s, which takes no lock (L.console.35). Ledger 3366.

### 2.2y Waiting for a kernel spin lock, and the holds a program can ask for (F-95, 2026-10-24)

**The wait.** `crate::sync::SpinLock` (`ferrix_sync::PreemptSpinLock`)
raises the processor's preemption count and then waits for its ticket.
Ticket order is arrival order, so each waiter waits out every hold queued
ahead of it, and does so with the count raised: its processor makes no
scheduling decision (`preempt_on_irq_exit` returns at once) and wakes none
of its timed sleepers, which only that processor's own decision wakes
(`choose_next` → `wake_sleepers`). With one multi-threaded program
contending one of its locks, that is every processor at once. The
interrupts still run and the waiting is charged to the waiting task, so
nothing here is unaccounted; it is time every other job loses. No bound is
stated for it today. `cargo xtask test-stall` measures it, reporting and not
asserting (`docs/PREEMPTIBLE-WAITS.md` §5).

**The holds a ring-3 program can make long.** A census, started
2026-10-24 and not yet complete; each is one hold of a spin lock over work
that grows with what the program asked for:

| Hold | Lock | Grows with | Measured (KVM, 1 GiB, loaded host, I1) |
|---|---|---|---|
| `AddressSpace::fork`'s copy | the space's | pages and regions of the parent | 157-391 ms |
| `Vmo::fork`'s page-map copy | the VMO's pages | committed pages | 155-379 ms |
| `give_back` → `Vmo::take_range` | the space's, then the VMO's pages | pages unmapped | 134-459 ms |
| `unmap`'s `take_down` | the space's | pages unmapped | not measured apart |
| `Vmo::detach`'s `retain` | the VMO's mappers | mappers, dead ones included | 128 s seen with zombies unreaped |
| the shootdown turn, held for the answers | `smp::SHOOTING` | the slowest answer (FM-10) | 344-1,107 ms, host-starved processors not ruled out |

Each such hold is a stop of its own processor and, through the wait above,
of every processor whose task waits for the lock.

**The dead, freed on a busy machine** (L.sched.95, since 2026-10-24). A dead
task's stack, and the address space and mapper entries its last reference
holds, used to be freed only by an idle task. Under a load that leaves no
processor idle the dead piled up by the thousand, and `Vmo::detach`'s and
`attach`'s `retain` over an object's mapper list -- O(live mappers), with
every unreaped task's space still live -- grew with them: 128 and 190 s of
raised count on two processors under a forking program. Now a task that
ends while more than `REAP_AHEAD` (64) dead wait, and that may block, frees
up to `REAP_BATCH` (16) of them first, under at most one shootdown,
preemptible as `sched::reap` is. The list is the machine's, so the ending
task may free another job's dead, and a reaped task's drop may be the last
reference to another job's process and address space: that teardown, which
grows with the space's pages, then runs in the ending task and is charged to
its job. **A bounded residual:** one exit pays for at most `REAP_BATCH`
other jobs' teardowns (per-job reaping is backlog p2-157). The dead waiting then stay at most `REAP_AHEAD` plus one for each
processor ending a task at once, whether or not any processor is idle, and
`retain`'s length is the live mappers, bounded by the job's task quota
(F-35) and the machine's. The fix, its bounds and
its checks are `docs/PREEMPTIBLE-WAITS.md`'s, under the consultant's
verdict of ledger line 3950.

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

### 2.2r An RMRR's identity domain at bring-up (`iommu::bring_up_vtd`, K2)

Before any program runs, once per VT-d unit whose RMRRs are honoured
(`L.iommu.73`), `vtd::Unit::attach_identity` maps each RMRR page at its own
address: at most 2^18 pages an RMRR (`ferrix_paging::vtd::rmrr::MOST_PAGES`,
1 GiB; the N150's is 17,408), and at most as many RMRRs as the DMAR's
length holds (24 bytes and a scope each, the table's length a 32-bit
field), each page one `mm::map_io` whose table writes are
cleaned and published as every domain map's are, then one flush. The
tables take at most one frame per 512 pages plus the levels above them
(35 frames on the N150), from the frame allocator, kept for the boot.
`plan_rmrrs` before it reads each page's frame state once
(`mm::frame_state`, the allocator's lock, a leaf, held per read) and
compares the RMRR with every memory-map region. The unit's `tables` lock
is held only inside `attach` and each map's table walk; the unit's
command gate across the flush and its wait, which is bounded as 2.2i
says. Nothing else runs then, so this is boot time, not a bound any
program waits on. At run time an identity domain adds to `Domain::pin`
and `unpin` one comparison of each page against its RMRRs (one range on
the N150), outside every lock.

### 2.2s An AMD-Vi unit (`iommu::amdvi`, 2026-10-24)

Memory, per unit, taken once at stage 10's bring-up and never given back:
the device table, one 2 MiB block (order 9, 65,536 entries of 32 bytes);
one empty table frame every blocked entry of a translating unit points at;
the command buffer's frame (256 commands of 16 bytes), the event log's (256
records) and the completion word's. Per function, at its first vector, one
4 KiB frame for its interrupt table (512 four-byte entries, 2 KiB used),
never given back, as its vectors are not; per domain, its root frame and
the tables below it, as VT-d's. Nothing is allocated per interrupt.

Time: every command batch is held inside the unit's `gate`, entered with
interrupts on wherever the caller may block; the hold is the commands'
writes, one tail register write and the wait for the completion word,
bounded by the unit's patience, 100 ms (2 ms for check R7's wait that
stores nothing). A batch carries at most 252 commands. Bring-up
invalidates every devid the IVHD names, two commands each: on nazuna
(devids 0x0003-0xFFFF) about 131,000 commands in about 520 batches, each
waited for; on QEMU a few dozen. Both halves of a device table entry are
written under the unit's `tables` lock, an `IrqSpinLock` held for the
writes only, never across a wait. A fault take reads one event record
under the `taken` lock. A function's first vector takes the unit's
`remaps` gate across its table's allocation, the entry's write and two
batches.

### 2.2y The crash hold (`fs::crashes`, load ring, 2026-10-24)

Tombstones step 2 (`docs/LOGGING.md` §21.7; the consultant's OK IF, ledger
lines 3501 and 3524). The hold is load-ring code; no core or item line
changes, and nothing here is on a path the item's own bounds name.

Time. **A fatal user fault may delay its process's end by at most 2 s
while `/dev/ferrix-crashes` is armed; at most 16 processes are held at
once.** The deadline is fixed at the fault (`HOLD_NANOS`, from the faulting
thread's own `wait_until_deadline`, no new timer) and no read extends it.
Within it the line waits at most `SETTLE_NANOS`, 200 ms, for the other
threads to park. Unarmed, a fault reads one atomic more (crash-1). A
crashing driver's or service's restart can therefore start up to 2 s later
while a collector runs (`docs/INIT.md` and devmgr's restart, T0's driver
restart); pid 1 is no exception.

Locks. The ring is one `SpinLock` (`RING`) over a fixed array of 16 slots,
held only to take, fill, read or free a slot -- never across a wake: the
reader's close collects the held pids under it and wakes their processes
after. The exclusion scan reads the faulting process's descriptor table
under that table's own lock, bounded by `RLIMIT_NOFILE` entries, with no
other lock held, and only while armed; it fails closed (a table that holds
the reader's description is not held). The faulting thread waits with no
lock held (a debug assertion on the preemption count).

Memory. None on the fault path beyond today's `crash_identity`: the ring is
static, the line is written on the stack (160 bytes). An open allocates its
one small object (`fallible::try_arc`, `ENOMEM` leaves the device unarmed).

### 2.2z Product signing's verifier (`ferrix-sigverify`, item, 2026-10-24)

`docs/SIGNING.md` §2.3 and §8.5. These are the verifier's own bounds,
`L.sigverify.11` (time) and `L.sigverify.12` (stack). No kernel path
reaches the verifier: the loader, init and `devmgr` call it from S4 on.

Memory. **No allocation.** Every buffer is one the caller lends, the
largest being the 4 KiB block. The verifier's own state is a few SHA-256
states and 26-byte digit strings on the stack. The stack bound in bytes
comes from the loader's build in S4; until then `L.sigverify.12` stays in
the traceability baseline.

Time. The work is bounded by the type codes. For the product set it is
at most p × (2^w − 1) = 765 Winternitz steps per level, plus the K, Q and
h tree hashes.

**Measured on the DK1 (slice S2, 2026-10-24).** The board is an
STM32MP157D (Cortex-A7), its MPU at 650 MHz as U-Boot reports and the
cycle counter confirms (650,001 kHz against the 24 MHz generic timer).
The program ran from U-Boot's prompt with `go`, so it ran in the state
the UEFI loader runs in: U-Boot's MMU, I-cache on, D-cache write-through.
The verifier was built as the product builds it (`opt-level = 2`, fat
LTO, one codegen unit, frame pointers). PMCCNTR counted every cycle.
Cold means the first call after the program was loaded; warm is the min,
mean and max of the 10 calls after it.

**What was measured, exactly.** This was a manual board run, not a gate
row (board use: the customer's approval of 2026-10-24, "DK1 is plugged in
and can be tested", relayed by the product owner).

* The source was `ferrix-sigverify` and `ferrix-sha256` as landed in
  546080e4b. Their files are byte-identical on this landing.
* The build used Rust 1.97.1 for `armv7a-none-eabi`, with
  `-C relocation-model=static -C force-frame-pointers=yes` and the
  product's release codegen. It was linked at 0xC4000000 and made a raw
  binary of SHA-256
  `8397512cfb9d5a91d92a7ee12b3be807bb0fdaf41a5f68bd6dbc1d6e932aaca3`.
* The figures are the third run in the console log (lines 498-522 of
  `console-2026-10-24.log`). Two earlier runs were not used: one carried
  a placeholder in place of the chosen manifest, so it verified nothing;
  the other ran stale instructions, because `loady` does not invalidate
  the I-cache, and stopped on a data abort that reset the board. The
  cited run invalidated the I-cache first (`icache off; icache on`).
* "Cold" is the first call after the program was loaded over serial. It
  may be warmer than a boot from the card. The board ran on a 500 mA
  supply, which U-Boot warned about; at the confirmed 650 MHz this does
  not change cycle counts.

| Case | Chain steps | Cold | Warm (min / mean / max) |
|---|---|---|---|
| RFC 9858 A.4, `hss_verify` (H20, W4, L = 1) | 435 | 4,971,405 cycles = 7.65 ms | 4,956,373 / 4,956,413 / 4,956,612 cycles = 7.62 ms |
| A full 4 KiB manifest `verify`, H20 W4 L = 1, its body chosen from 2 million as needing the most steps | 555 | 6,214,025 cycles = 9.56 ms | 6,210,449 / 6,210,478 / 6,210,696 cycles = 9.55 ms |

From the two cases, one Winternitz step costs about 10,450 cycles (one
47-byte SHA-256 call measured 9,356 cycles on its own), and the fixed part
(K, Q, the 20 tree hashes and the parse) costs about 0.41 M cycles.

**The upper bound for one level, computed from those two measured points
and not itself measured**, is all 765 steps: about 8.4 M cycles,
**12.9 ms**. It is conservative on two counts:

* the slope includes the manifest body's own hash, which the second case
  has and the first has not;
* the checksum keeps any valid signature below 765 steps.

A two-level key (an HSS option, §4.3) would take about twice that,
25.9 ms. **The bound is 50 ms.** It holds with a margin of 3.9× for the
chosen L = 1, and 1.9× for L = 2. The design's estimate was 7.5 ms at
eBACS's SHA-256 cost. One SHA-256 call here costs 9,356 cycles against
eBACS's 3,044, slightly over the 3× slowdown the budget assumed; the
bound still holds with a 3.9× margin.

`L.sigverify.11` stays in the traceability baseline (ledger 3843 (12)).
This one manual run, with its extrapolated worst case, does not verify
it. It leaves the baseline in a later landing that brings:

* the bench into the tree, with a TOOLS.md row and its load, run and
  parse script;
* the criterion rewritten to "measured at 555 steps or more in the
  loader's cache state, with the 765-step bound computed from the measured
  step cost and fixed part, under 50 ms with a margin of at least 2";
* a run on matching sources.

A backlog row (`docs/backlog/p2.md`) owes that.

**SHA-256 throughput over 32 MiB of DRAM, as landed in S1:** 6.594 s,
127.7 cycles per byte, against eBACS's 28.24. That is 4.5×. It is the cost
of hashing a slot's blobs, not of the signature.

**Why it was slow (investigated 2026-10-24, before S4).** The same bench
program, on the same board in the same state, measured memory, cache and
codegen apart, against references. One raw binary held all the variants
(`shabench2.bin`, SHA-256 `26eb14af6ddff9c7…`); each agreed with the
others' digest. Figures are cycles per byte over 32 MiB of DRAM. The
figure over 4 KiB held in L1 is within 3% of each.

| What | cycles/byte | 32 MiB |
|---|---:|---:|
| Reading 32 MiB (a word XOR loop) | 0.89 | 0.05 s |
| Copying 16 MiB | 1.07 | -- |
| `ferrix-sha256` as landed in S1 | 127.7 | 6.59 s |
| `ferrix-slots`' SHA-256 before S1 | 57.0 | 2.94 s |
| Textbook Rust (64-word schedule) | 52.4 | 2.71 s |
| Unrolled rounds over a 16-word ring | 44.5 | 2.30 s |
| `sha2` 0.10.9, its software backend | 40.4 | 2.08 s |
| CRYPTOGAMS `sha256-armv4` (from `ring` 0.17.14), integer | 30.2 | 1.56 s |
| CRYPTOGAMS `sha256-armv4`, NEON | 28.5 | 1.47 s |
| The 157D's HASH1 peripheral, fed by the CPU, no DMA | 7.4 | 0.38 s |

* **Memory and cache are not the cost.** A read runs at under one cycle
  per byte, and in-cache and DRAM figures agree. U-Boot's DRAM sections
  are write-back (TEX 000, C 1, B 1), although `dcache` prints
  "writethrough". Remapping the region write-back write-allocate changed
  nothing.
* **Codegen settings are not the cost.** `-C target-cpu=cortex-a7`
  changed nothing; `opt-level = 3` gained 2.5%. Thumb-2 was not measured
  (no prebuilt library for `thumbv7a-none-eabi`), and the loader builds
  for ARM mode.
* **The cost was ours.** S1's rewrite shifted the 16-word schedule one
  place every round (48 × 16 moves per block) and copied input byte by
  byte into the block buffer. That made it 2.2× slower than the code it
  replaced.

**The fix (S4).** Whole blocks are compressed straight from the input,
and the 64 rounds are written out over a 16-word ring with constant
indices. It runs at **44.4 cycles per byte, 2.29 s per 32 MiB**, within
10% of `sha2`'s software backend. The CAVP vectors pass unchanged.

The figures below are from the landing source: `src/lib/crypto/sha256/src/lib.rs`
of SHA-256 `c7847ca9be82eeb353aaf9dae7a41320c88f84d1792dba12f9c2022d271214fd`,
built with Rust 1.97.1 into `shabench4.bin` (`b634bda22106631036db48b2…`,
`run4.out`) and `sigbench-s4b.bin`, the S2 program on that source
(`da0434ca500a739c9f9f15edd0a50c18…`, `run-sig-s4b.out`). An earlier build
of the fix, taking `&[u8]` in place of `&[u8; 64]`, measured the same to
within 0.2% (`run3.out`, `run-sig-s4.out`). The verifier is 2.7× faster
than S1's:

| Case | Chain steps | Cold | Warm (min / mean / max) |
|---|---|---|---|
| RFC 9858 A.4, `hss_verify` | 435 | 1,870,902 cycles = 2.88 ms | 1,832,355 / 1,832,445 / 1,833,089 cycles |
| The near-worst 4 KiB manifest `verify` | 555 | 2,316,563 cycles = 3.56 ms | 2,313,030 / 2,313,049 / 2,313,186 cycles |

One step costs about 4,006 cycles (a 47-byte SHA-256 call is 3,370, 1.1×
eBACS), and the fixed part about 0.09 M. **The computed 765-step bound is
about 3.15 M cycles, 4.85 ms**, a margin of 10× against the 50 ms bound
(about 9.7 ms and 5× for L = 2). The S2 figures above stay as the record
of the landed S1 code.

**On x86-64 (slice S4, the signed A/B gate).** The loader prints each
signed start's decision in counter ticks: header decode, policy block
parse and one H15 manifest verify, under KVM on nazuna's Ryzen 9 9900X,
whose invariant TSC runs at its 4.4 GHz base clock. Eight starts of
`test-ab-boot-signed` took 1.73 to 2.59 M ticks, about 0.39 to 0.59 ms
(`~/.local/share/ferrix/signing-bench/s4-signed-local7.log`; the landing's
gate row repeats it). That is a development build of the loader on a
development H15 key; the bound above, from the DK1, stays the worst case.

**Slot hashing in the loader** is then about 2.3 s per 32 MiB of kernel
and initramfs on the DK1, still not AB-BOOT §4's "well under a second".
What closes the rest:

* Assembly (CRYPTOGAMS, 1.5 s) adds a SOUP unit in the item for a 1.5×
  gain, so it is not taken.
* HASH1 (0.38 s) is 6× faster than the fix. It is reachable from the
  normal world at U-Boot time: its clock is RCC_MP_AHB5ENSETR bit 5, its
  registers at 0x54002000, and the DK's OP-TEE does not secure it (ETZPC
  id 8). A peripheral in the trust path needs its own argument, which is
  for the consultant: a fault in it gives a wrong digest, which the
  manifest's signature does not cover. The comparison would fail closed
  on most faults, but a peripheral that returns the expected digest
  regardless would pass any image. S4 uses software and states the cost.

The S2 program and its data, the raw binaries with their SHA-256, and the
board's console log are kept in `~/.local/share/ferrix/signing-bench/dk1-s2/`;
the investigation's bench, its variants, binaries (`SHA256SUMS`), runs and
`FINDINGS.md` in `~/.local/share/ferrix/signing-bench/dk1-sha/` (ledger 3988).
The board was not written: the program went over the serial line with
`loady`, and the board was left booted as it was found.

## 3. Why F-24 is not closed, and F-23 is

A document that says "the property does not hold" is not a closed finding, and
recording it as one would be the exact failure this directory exists to avoid.
F-24 is restated against this analysis rather than struck out.

F-23 closed on 2026-09-26 because what it described stopped being true, and a
gate and a boot check say so: no allocation in the item's product code is
fatal except at bring-up. What it did not describe, a bound, stays unclaimed,
and says so in §1.5.
