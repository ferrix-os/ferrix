# Robustness: no program stops the machine

**The customer's order (2026-11-08):** "find out how we can make ferrix
more robust, because a process in ring 3 should never crash the whole
kernel".

This document names the classes of bug that let a program take the
machine down, the evidence for each from the N150 box on 2026-11-07/08,
how big each class is today, and the mechanism that closes it. It is the
systemic answer. The fixes for single sites belong to the sessions named
in §8, and this document does not repeat them.

Each class says what Linux does about it. That part was checked against
Linux v7.0's own source (fetched 2026-11-08 to
`~/.local/share/ferrix/robustness-ref/linux-v7.0/` from git.kernel.org).
It is cited by file and function, and it was read, not copied.

The rule it follows is the customer's of 2026-10-22: keep the kernel slim.
Each mechanism below is a small, typed thing in the kernel. Any choice of
*whom* to kill or restart, and when, is made in ring 3.

**Where it stands (2026-11-08, robustness).** This document, and the first
guard, are on branch `docs/robustness`. The guard is
`tools/common/check/check-ring0-alloc.py`, run by `cargo xtask check` as
`ring-0 allocation`. It ratchets class A's 1,382 sites (§2.3). The
certification consultant's verdicts on both are recorded in §9. R1 is
landed. R2 to R13 are open BACKLOG rows.

## 1. The principle, and what "the whole machine" means

There are three ways for a program to take the whole machine down:

1. **A kernel stop.** A panic (`fatal!`, FX-0008) or a lockup report.
   Nothing above the kernel recovers from one, because `panic = "abort"`
   leaves nothing to unwind to.
2. **A system-wide stall.** The kernel keeps running, but nothing in user
   space gets a turn. Init stops feeding `/dev/watchdog`, and FX-9008
   resets the box. The stall itself is the failure, and today the reset is
   its only answer.
3. **A global service failure.** One thing every program needs fails for
   everyone: the root filesystem answers `EIO`, the compositor dies, or
   the GPU driver wedges. Programs then die one after another, init among
   them.

Each of the three has to be closed separately. A kernel that never panics
but stalls for 15 s under one program's teardown is not robust.

## 2. Class A: an allocation that cannot fail, on a path a program drives

### 2.1 Evidence

* FX-0008 "memory allocation of 131072 bytes failed", twice: catch3 #2,
  and catch4 `eventlog-fault-relanding-1121.txt` #7 (11:17, under load).
  The first was ferrix-btrfs-vfs `Scratch::new`, found by reading the
  code. The second record names no caller, because the trace goes to the
  port only (BACKLOG, ledger 8043 MK.5).
* FX-0008 for a 204-byte path clone in `openat`, in
  `test-btrfs-pressure` (owned by btrfs-pressure).
* `docs/BTRFS-PRESSURE.md`: develop stopped with "16384 bytes failed" in
  `Shared::write_back_in_slices`, and with "192 bytes failed" in
  `Node::get`'s insert under the `nodes` spin lock.

### 2.2 Why it is a class

The kernel's `GlobalAlloc` reports an empty heap as null. A `Box`, `Vec`,
`String`, map or `format!` turns the null into the allocation error
handler, and that handler panics. The certified item has been held to
fallible allocation since F-23 (`check-fallible-alloc.py`, 43 sites
left). The rest of ring 0 was never held to it: the load ring's Linux
layer, the filesystems, sockets and interfaces, and the libraries the
kernel links. Every program reaches those paths through `openat`,
`read`, `ioctl`, `sendmsg`, a page fault on a file, or a driver's IPC.

Two things make each such site worse than a count suggests:

* **The rescue does not run everywhere.** `mm::heap_or_reclaim` and
  `heap_or_kill` (L.mm.136-138) reclaim clean cache and then kill, but
  only where the caller may block. An allocation under a
  `sync::SpinLock` can only take what the headroom refill left
  (`refill_headroom`), and so can one in an interrupt or inside a
  reclaim. The inserts under spin locks are the "192 bytes" case.
* **A large block is a contiguous run.** A 128 KiB request is an order-5
  run of frames. With memory fragmented there may be no such run even
  while frames are free above the watermark. In that case `heap_or_kill`
  correctly kills nobody, and the infallible caller panics anyway. The
  box fails this way after hours at the floor.

btrfs-pressure's findings (44cb219e1, 3c4498db2, `docs/BTRFS-PRESSURE.md`)
make the second and third points exact:

* **Under a spin lock there is neither reclaim nor kill.** With the
  preemption count at 1, `heap_may_reclaim()` is false. So any `Box`
  made under a `sync::SpinLock` guard panics with FX-0008 once the heap
  is empty. The `openat` stop was `Dentry::insert_looked_up` allocating
  under the children's lock. The rule for this sub-class is to allocate
  before the lock, or to allocate fallibly under it. tmpfs's maps and
  `move_child`'s map insert under a lock are still unaudited, and they
  are first in R7's queue.
* **The kill allocates from the heap it is meant to refill.**
  `process::live()` builds two `Vec`s, so at an empty heap the kill
  answers Refused. btrfs-pressure is finishing the fix, which serves the
  killer from the per-processor reserve (`park/heap-kill-reserve`,
  a4bcc40e0). Its general form is a rule: **a recovery path allocates
  nothing, or only from a reserve kept for it.** §6.2 (3) applies the
  same rule to filesystems.
* **Large blocks and fragmentation.** A request above 2048 bytes takes a
  buddy block of its order (128 KiB is order 5). L.mm.136 reclaims
  scattered clean pages and retries at most 8 times. There is no
  compaction, and `heap_or_kill` kills nobody while free frames are above
  the watermark. So a 128 KiB request fails with gigabytes of cache
  free, as #7 at 11:17 did.

### 2.2a What Linux does (v7.0)

* **Failure is the default, and "cannot fail" is a flag.** A Linux
  allocation returns NULL unless the caller passes `__GFP_NOFAIL`. That
  flag's contract (`include/linux/gfp_types.h`) is that the allocation
  must be able to block (`__GFP_DIRECT_RECLAIM`), must never be used where
  sleeping is forbidden, and is not supported for buddy orders above 1.
  Larger requests are told to use `kvmalloc()`. So Linux has no
  equivalent of Ferrix's implicit infallible `Box` under a spin lock.
  The kernel's coding rule is that a `GFP_ATOMIC` caller handles NULL.
* **Atomic callers get a reserve, by flag.**
  `mm/page_alloc.c:__zone_watermark_ok` lets `ALLOC_MIN_RESERVE`
  (`__GFP_HIGH`) use half of the `min` watermark. `ALLOC_NON_BLOCK`
  (`GFP_ATOMIC`) gets a quarter more, and `ALLOC_OOM` (an OOM victim on
  its way out) gets half again. `gfp_to_alloc_flags` turns the caller's
  context into these bits. Ferrix has one watermark, and it infers "may
  not block" from the preemption count (`heap_may_reclaim`) instead of
  being told. That inference is why every allocation under a
  `SpinLock` is a no-reclaim, no-kill one.
* **Scoped contexts, not inference.** `include/linux/sched/mm.h`:
  `memalloc_nofs_save`, `memalloc_noio_save` and `memalloc_noreclaim_save`
  mark a region of code, and `current_gfp_context` strips `__GFP_FS`/`IO`
  inside it. A filesystem holding its own locks is then never re-entered
  by reclaim. Ferrix's "inside a reclaim" mark (FX catalog
  `RECLAIM_IN_RECLAIM`, the kill "marked inside a reclaim") is the same
  idea for one case. R6 makes it a general scope.
* **Mempools for the paths that must make progress.**
  `mm/mempool.c:mempool_alloc_noprof` first tries the backing allocator
  without direct reclaim, then takes a preallocated element, then, if it
  may block, waits for one to be freed and retries. The writeback and
  block paths use this, so I/O that frees memory never needs fresh
  memory to start. Ferrix's `mm/reserve.rs` reserved sections are the
  same shape for the item. §6.2 (3) and the kill that allocates (§2.2)
  are the cases that need one.
* **The OOM killer does not help a large block.**
  `mm/page_alloc.c:__alloc_pages_may_oom` skips the kill for
  `order > PAGE_ALLOC_COSTLY_ORDER` (3). The reason given is that
  killing a task will not produce a contiguous run. Instead
  `__alloc_pages_slowpath` tries direct compaction first for costly
  orders, and a caller that can take virtual contiguity uses `kvmalloc`
  (`mm/slub.c:__kvmalloc_node_noprof`, falling back to vmalloc).
  Ferrix's `heap_or_kill` declining to kill above the watermark is
  therefore Linux's behaviour. What Ferrix lacks is the other half:
  neither compaction nor a `kvmalloc` fallback. R6 adds the fallback.

### 2.3 The audit (2026-11-08, develop 3c4498db2)

`python3 tools/common/check/check-ring0-alloc.py --report` gives these
counts. They use `check-fallible-alloc.py`'s token patterns and markers.
Test modules and test files are left out, and a marked site is not
counted.

| Scope | Unmarked allocating calls | Files with any | `.clone()` (not gated) |
|---|---:|---:|---:|
| The certified item, and the 6 load files it reaches (F-23's gate) | 43 | 6 | 40 |
| **Ring 0 outside the item: the new gate's scope** | **1,382** | **176 of 400** | **127** |
| of which the kernel's load ring: `fs/` | 376 | 30 | 44 |
| `interfaces/` (display, render, input, block ring, audio) | 233 | 14 | 9 |
| `syscall/` | 121 | 33 | 29 |
| `net/` | 70 | 10 | 2 |
| kernel extension crates (boards, logctl, net ring) | 24 | 7 | 1 |
| of which the linked libraries: `ferrix-vfs` | 144 | 12 | 22 |
| `ferrix-net` | 87 | 13 | 0 |
| `ferrix-procfs` | 74 | 11 | 0 |
| `ferrix-btrfs-vfs` | 46 | 3 | 8 |
| `ferrix-block`, `ferrix-sysfs` | 39 each | 5, 7 | 2, 0 |
| 24 other crates | 1 to 15 each | | |

The files with the most are `ferrix-vfs` `namespace.rs` (64),
`fs/procfs/render.rs` (56), `fs/sysfs.rs` (50),
`interfaces/display/mod.rs` (45), `interfaces/display/drm.rs` (41),
`interfaces/render/mod.rs` (35), `ferrix-vfs` `tmpfs.rs` (32),
`fs/devfs.rs` (31), `fs/socket.rs` (29) and `ferrix-btrfs-vfs` `rw.rs`
(28).

**How much a program can reach.** Nearly all of it. The load ring *is*
the system-call personality, and an interface's code runs on a driver's
or a client's message. A call graph from the entry points (the syscall
table, the fault handler, the interface dispatchers) would say exactly
which sites are reachable. That work is R7's first step, not this
document's. The honest figure today is "up to 1,382, nearly all of them
reachable". The token scan also misses `.clone()` of an owning type,
`.into()` conversions, `?` into a boxed error and callees. That is why
R12, a fault-injection row, is in the plan.

### 2.4 What happens on failure

There is one rule, with one exception at each end:

* **A system call answers `ENOMEM`.** An allocation made for a request
  fails that request, and nothing else. This matches Linux's `GFP_KERNEL`
  allocations under `__GFP_ACCOUNT`, and the item's rule.
* **A path with no error to return kills the program it serves.** A page
  fault, a signal delivery or an exit's own bookkeeping cannot answer
  `ENOMEM`. Such a path ends the requesting process the way the OOM kill
  does (`SIGKILL`, `oom_kill` counted), so its memory comes back.
  `object::oom` already does this for a fault's frame. Class A extends it
  to the kernel heap a fault needs.
* **Never a panic.** `FATAL-ALLOC:` stays what it is in the item: boot
  only, before the first program, where there is nothing to return to.

For the large-block case there are three candidate mechanisms:
compaction, a fallible-only rule above a page, or non-contiguous large
buffers. R6 takes the third: kernel requests above one page are served
virtually contiguous, from order-0 frames (`vmap.rs` already maps the
stacks this way). The heap's order-n runs then stop being something a
program's request depends on, and nothing has to move pages. Compaction
is rejected because it is a large mechanism in the core that only
lowers the odds. The fallible-only rule is kept as well: a site that
asks for more than a page through `vmap` must still handle the refusal,
because the R1 ratchet sees the call either way.

**Allocation under a live guard, found at run time (R13).** A token
scan cannot tell whether a guard is alive at a call. The kernel can. In
debug and box images, the allocator counts every allocation made with
the preemption count raised, and keeps a small ring of the callers'
return addresses, which `fatal!`'s trace code can already walk. An
xtask row boots a workload and prints the top sites. A static form, the
script tracking a `let g = x.lock()` binding to the end of its block,
follows only if the run-time list proves too slow to converge.

### 2.5 The mechanical guard (R1, this landing)

This was the decision: **a ratchet script**, the same kind of gate as
F-23's, over a per-file baseline. Three other options were rejected:

| Option | Why not now |
|---|---|
| clippy `disallowed-methods` on `Box::new`, `Vec::push` and so on | It is typed and has no false positives, which is better than tokens. But it is workspace-wide or per-crate in `.clippy.toml`, and on today's code it needs 1,382 `#[expect]`s at once. **Adopted per crate later:** once a crate's count reaches 0, its `.clippy.toml` gains the list, and the ratchet entry goes (R7). |
| `try_` APIs only, in "syscall crates" | The kernel's load is one crate, `ferrix-kernel-load`, so there is no boundary to put the rule on. |
| An allocator that never fails (a reserve big enough for everything) | It moves the stop instead of removing it. A reserve is right for atomic sections, which is R6, and wrong as a policy. |

`check-ring0-alloc.py` covers every load-ring product file of the
kernel's source that F-23's gate does not scan, and the product files of
every workspace crate the kernel links (the shipping dependencies of
`ferrix-kernel`, `-core`, `-item` and `-load`, followed through the
workspace), apart from the item's crates and `ferrix-fallible`. The
baseline is `tools/common/data/ring0-alloc-baseline.json`. The gate
fails in two cases: a file's unmarked count rises, or a new file appears
with any unmarked call. A newly linked crate counts as new files, because
linking a library into ring 0 is when its allocations become the
kernel's. A fall in a count passes, and is reported. This is deliberately
looser than the item's ratchet. The load changes in most landings, and
requiring every conversion to re-record a shared file would put a
conflict in most rebases. The next `--record`, which any landing may run,
closes the allowance a fall leaves. An R7 landing records its own
files' falls in the same commit. It runs in about a second.

`--record` only lowers (the consultant's R1.2). It writes each file's
smaller count and drops a file at 0. While any count is above the
baseline, it writes nothing and fails. A raise or a new file is a hand
edit of the JSON, and that edit is reviewed. A commit that only lowers the
baseline says "Certification: ring-0 baseline lowered only (ledger
8202)".

**The residual, stated (R1.3).** Between two records, a removal and an
addition in the same file pass with nothing printed. The consultant
showed this by swapping a `format!` for a `Box::new` in `pipe.rs`. A
fall is an allowance until the next `--record`. A change to
`check-fallible-alloc.py`'s patterns or markers re-records this baseline
in the same commit, under review (R1.6).

It is not `check-kernel-alloc-audit.py` (K16's map of *who pays* for
each kernel file's allocations, marked or not). That gate fails on any
count that moves, and it is answered by changing the count. This one asks
only whether a failure can be reported.

**Negative controls.** These hand runs are diagnosis, not evidence: a
planted `Box::new` in `src/kernel/src/fs/pipe.rs` failed the gate with
"pipe.rs: 10 -> 11", exit 1, and a `vec!` planted in `ferrix-btrfs-vfs`
`rw.rs` failed it too. The `gate.sh control` runs that count as evidence
are cited in the landing's commit. The self-test covers the dependency
walk (renames, target tables, build dependencies excluded), the count,
and `--record`'s refusal of a rise and of a new file.

The evidence, on 1ceeac1d2 through `gate.sh control ... --expect-host
... -- check`, with each edit undone afterwards:

* (a) `const _: fn() -> Box<u8> = || Box::new(1);` in `fs/pipe.rs` gave
  `control: FIRED (host): src/kernel/src/fs/pipe.rs: 10 -> 11`
  (`~/.local/share/ferrix/logs/queue/rob-r1-ctl-pipe.log`).
* (b) the same with `vec![1]` in `ferrix-btrfs-vfs`'s `rw.rs` gave
  `control: FIRED (host): src/lib/fs/btrfs/vfs/src/rw.rs: 28 -> 29`
  (`rob-r1-ctl-rw.log`).
* The `check` row passed: `run: PASSED`, with `ring0-alloc: 1382 ...
  baseline 1382` (`rob-r1-check.log`).

### 2.6 R6's design: requests above a page from single frames

R6 is the remedy ledger 8470 chose for W1.2 and for #7's 128 KiB stop. It
answers RD.6 (a)-(e) and (g) (ledger 8222) and CR.4 (a)-(i) (ledger
8474). R6's second half, the accounted reserve for allocations under a
spin lock (RD.6 (f)), is split out as **R6b**, which owes its own design
review before code (§7, ledger 8488 R6.6). CR.4 does not need it, and it
generalizes `mm/reserve.rs`, which is a different change.

The consultant reviewed the first version of this section: **OK IF
R6.1-R6.13** (ledger 8481). This revision answers R6.1 (x86-64 keeps a
freed block's frames until its shootdown), R6.2 (the lazy list is
bounded by a kicked purge), R6.3 (no self-deadlock on `TABLES`), R6.4's
order, R6.7 (the check) and R6.8 (five ids). Where it departs from a
condition's letter it says so, marked **Departure**. The consultant read
this revision before code: **OK IF R6F.1-R6F.9** (ledger 8503). It
accepted the three departures, and this text carries R6F.3's alignment,
R6F.4's descriptor stores and R6F.7's purge before a kill. The other
conditions are checked at the code review.

**What Linux does (v7.0, read, not copied).**

* `mm/slub.c:__kvmalloc_node_noprof` (the `kvmalloc` body; `mm/util.c` no
  longer holds it in v7.0) tries `kmalloc` first, with
  `kmalloc_gfp_adjust`. For a request above `PAGE_SIZE` that function
  clears `__GFP_DIRECT_RECLAIM` and `__GFP_NOFAIL` and adds
  `__GFP_NOWARN`: one cheap try at a contiguous block, no reclaim. The
  comment gives the reason: a contiguous block "is less likely to fragment
  multiple larger blocks" over the long term than the vmalloc fallback.
  Only then does it call `__vmalloc_node_range_noprof`. A sub-page request
  never falls back. An alignment above what `kmalloc` guarantees is not
  honoured.
* `mm/vmalloc.c:__vmalloc_node_range_noprof` takes a `vmap_area`
  (`alloc_vmap_area`, an augmented rbtree of free space) with a guard
  page unless `VM_NO_GUARD` ("your top guard is someone else's bottom
  guard"). `__vmalloc_area_node` then allocates the page array (bounded
  recursion, its comment says), the pages through `vm_area_alloc_pages`,
  and maps them with `__vmap_pages_range`. That last step **allocates
  page tables**, which ignore the caller's gfp and are held to it only by
  the scope API (`memalloc_apply_gfp_scope`).
* `mm/vmalloc.c:vfree` frees each page at once (`__free_page`) after
  `remove_vm_area` has unmapped the range with no flush
  (`free_unmap_vmap_area` → `vunmap_range_noflush`). The address range is
  put on a lazy list (`free_vmap_area_noflush`, counted in
  `vmap_lazy_nr`). When the count passes `lazy_max_pages()` (32 MiB times
  log2 of the processors) a worker runs `__purge_vmap_area_lazy`, which
  does **one** `flush_tlb_kernel_range` over the span of every lazy area,
  and only then makes the ranges free again. In interrupt context
  `vfree` defers the whole free to a worker (`vfree_atomic`, a lock-free
  `llist` through the freed memory).

Ferrix takes the fallback policy, the guard and the lazy purge. It takes
the free-the-frames-at-once order only where the architecture's own
invalidation is broadcast (both Arm builds), since H.MEM.17 forbids it
elsewhere (R6.1). It refuses allocating on the mapping path (CR.4 (b)),
and a worker for the free itself (a free here needs no lock that
interrupt context may not take, so it runs in place). Only the purge is
handed to a task, as Linux's `drain_vmap_area_work` is (R6.2).

**(f) Which requests, and alignment.** A request whose size is above
`PAGE_SIZE` (4096) and at most `2^MAX_ORDER` pages (4 MiB, today's
ceiling, unchanged), with an alignment of at most `PAGE_SIZE`. Requests
of 2049 to 4096 bytes are not included: they are a single frame (order 0)
on the buddy path already, and a single frame never needs a run. A larger
alignment stays on the buddy path, as today. That is Linux's rule: it
does not honour such alignments under `vmalloc` either. The block's base
is page aligned, which honours every alignment the path accepts.

**The policy is `kvmalloc`'s.** Every path that serves a heap request
today tries it first, as now. Only if the buddy refuses (no run of the
order) is the large path tried, with the same floor. So nothing changes
while a run exists. The fast paths, the size classes and a large request
that finds its run cost what they cost today. The large path is reached
only where today's code returns null. The exact order (R6.4, L.mm.141):

1. the reservation's heap (a heap scope's, or the job's), as today;
2. the large path, `Floor::Drawn` from that same reservation;
3. on x86-64, where lazy frames still hold that reservation drawn and the
   caller may block: one purge, then 1 and 2 once more (R6.1 (b));
4. a scoped request: the overrun, counted once per request, not per
   frame (L.mm.140);
5. the shared heap, through the caches and the drain, as today;
6. the large path, `Floor::Kernel`;
7. the reclaim (L.mm.136): after each reclaim, the shared heap and then
   the large path, `Floor::Kernel`;
8. the kill (L.mm.138): after each kill, the shared heap and then the
   large path, `Floor::Kernel`;
9. `reserve::draw` last (F-23, L.mm.139).

A request with no reservation starts at 5. No kill is asked while the
large path could serve: the large path runs at 6, before the reclaim and
the kill, and `heap_or_kill`'s early return while free frames are at or
above the watermark is kept, so a heap that is fragmented but has
plenty of frames never kills. The retries at 7 and 8 run with the scope
suspended, as today (L.mm.140). **Departure from R6.4's wording:** a
retry's large path takes `Floor::Kernel`, not the job's reservation. That
is what the retry's shared heap takes too (`KernelPages`), and nothing in
a retry draws from any reservation today. Drawing the job's there would
be a new rule the small heap does not follow. The reclaim at 7 asks for
`2^order` pages for a request above a size class, as today. The large
path needs `n` single frames, `n <= 2^order`, so the reclaim asks for at
least what the large path needs.

**The area.** At boot, right after `vmap::init`, one span is reserved from
the arena for the large path and never given back. **Its base and its end
are aligned to 2 MiB** (R6F.3), the span one page-level table maps on
x86-64, AArch64 with a 4 KiB granule and LPAE. So no page-level table of
the area is shared with an arena neighbour, whose free (`unmap_range`)
would give back a table it empties. The alignment is asserted at boot, a
`ferrix-paging` host test shows an unmap over an aligned range beside a
populated, leafless one gives back none of its tables, and the boot check
asserts at its end that every area table is still linked. It sits below the
arena's top, which KASLR moves, so it moves with it. Its size is
`clamp(RAM / 8, 64 MiB, 2 GiB)` on the 64-bit pair and
`clamp(RAM / 4, 16 MiB, 128 MiB)` on ARMv7-A, rounded to 2 MiB. The
boot report states it. If it cannot be set up (no frames for its tables at
boot), the large path is off and the kernel behaves exactly as today.

* **(b) Page tables are made at boot for the whole area** (CR.4 (b), the
  first option). A new `ferrix_paging::Mapper::populate` makes every
  table down to the page level over a range and writes no leaf. One
  table per 2 MiB costs `area / 512`: 4 MiB for a 2 GiB area, 256 KiB for
  ARMv7-A's 128 MiB. These tables are never pruned or freed. Their
  top-level entries exist before the first process root copies the
  kernel's slots (x86-64), and both Arm builds share them through `TTBR1`.
* **Address space is a buddy over 8 KiB units, in fixed storage.** It
  reuses `ferrix_frame::Frames`, the frame allocator's own buddy (host
  tested, O(`MAX_ORDER`) per call), over unit numbers instead of frames.
  A request of `n` pages takes `n + 1` pages of span (the guard is the top
  page), rounded up to a power-of-two count of units. A block is aligned
  to its size, and the guard above each block is the block above's
  bottom guard. The entries (16 bytes per unit) and the lazy list's side
  array (8 bytes per unit: the next lazy block and its order, and on
  x86-64 the head of its kept frames' chain) are allocated once at boot
  through `vmap::allocate`. For a 2 GiB area that is 6 MiB, so the total
  overhead is about `RAM / 1600` including the tables. The boot report
  prints the figures.
* **Nothing is allocated on the mapping path.** A request takes its unit
  block (one `IrqSpinLock` hold of the area's buddy, nothing taken inside
  it). It then takes `n` single frames, one at a time, through the same
  floor decision as the small heap, and writes their leaves into the
  existing tables. That write goes through `Mapper::map_range` with a
  pages-only mapper and a `PhysMem` whose `allocate_table` refuses, so a
  missing table is a refusal, never an allocation. There is no recursion into the heap, and so no re-entry
  (RD.6 (d)). The arena's `BTreeMap` is not touched after boot. A request
  refused part way (a frame short, a leaf refused) gives back every frame
  it took and puts its unit block on the lazy list, never straight back
  into the address buddy (L.mm.144, L.mm.48's sibling): leaves it wrote
  may be cached.
* **No self-deadlock on `TABLES` (R6.3): the large path never takes
  it.** `mm.rs`'s lock order permits a heap allocation while `TABLES` is
  held, so a large path that took `TABLES` from inside `GlobalAlloc` (or
  from `dealloc`) could spin on its own processor's hold. **Departure
  from R6.3's letter, removing its cause:** the area's tables are the
  large path's own. They are made at boot (above), never pruned and
  never freed, and nothing but the large path writes a descriptor inside
  the area: the arena reserved the span, so no `map_kernel`, unmap or
  prune reaches it, and `share_kernel_slots` copies root entries only.
  Each page-level leaf is written only by the owner of the unit block it
  lies in, and the area's buddy hands a block to one owner at a time. So
  a leaf write needs no lock: it is one aligned 64-bit store into a table
  that cannot change shape, by the one owner of that leaf. A reader under
  `TABLES` (`translate`, the sweeps) sees the leaf before or after the
  store, as a walker does. The lock doc says so: the area's lock is a
  leaf, taken alone, never with `FRAMES` or `TABLES`, and the large path
  takes neither `TABLES` nor `HEAP`.

  **Descriptor stores without a lock (R6F.4).** On x86-64 and AArch64 a
  leaf is one aligned 64-bit store, and readers read at the same width.
  On ARMv7-A, where a `u64` volatile store is not promised as one
  single-copy-atomic access, the area writes a leaf as two 32-bit stores
  in a fixed order: on a map the high word first, then the low word,
  which holds the valid bit; on a clear the low word first. Each is
  followed by the existing barrier. A walker therefore never sees a valid
  descriptor with a half-written address. A leaf found present at a map,
  or absent at `take_page`, means two owners of one block: it is counted
  (L.mm.144), never served, and asserted 0 at the end of the boot check.
  The check case R6.3 asked for stays,
  with the opposite answer: a request above a page made inside a
  `with_tables` body while the buddy refuses is **served** from the area,
  and freed there, and the boot goes on. If the consultant prefers
  R6.3's form (refuse when this processor holds `TABLES`), the owner word
  is one store in `with_tables` and one compare here; the free would then
  need the same refusal, which a free cannot give.
* **Publishing a fresh mapping costs no flush.** A leaf goes from
  invalid to valid. x86-64 caches no not-present translation. Arm caches
  no translation-fault entry, and the walker needs the store to be
  visible, so a new `arch::publish_tables()` issues `dsb ishst; isb` on
  both Arm architectures (what `flush_tlb` does before its `tlbi`) and
  nothing on x86-64. This is not `mm::map_kernel`, whose full
  `flush_tlb` per call would be a broadcast `tlbi vmalle1is` per
  allocation on Arm. The address range was purged (below) before it could
  be handed out again, so no processor holds a stale translation of it.

**(a) One entry choice, and (e) free routing.** The frames are taken by
the decision `heap_entry` makes for the small heap (L.mm.81, L.mm.140):

* in a heap scope, `take_frames(0, Floor::Drawn(scope))`, and past the
  reservation's room, counted as an overrun (`scope_overrun`) and then
  `Floor::Kernel`, unless the scope's check refuses the shared heap;
* for a job with a reservation, in task context, `Floor::Drawn(job's)`,
  then `Floor::Kernel`;
* otherwise, and always in interrupt context, `Floor::Kernel`.

`take_frames` tags each frame with the reservation it was drawn from, in
the hold that takes it. **Each frame is then freed through
`deallocate_frames(frame, 0)`, which gives it back to the reservation its
tag names (L.mm.80).** So the large path needs no record of its own of
where a frame came from, and a block that mixes drawn frames with
overrun frames gives each back where it belongs. L.mm.140's statement is
amended to say that a large allocation in a scope draws its frames
there. In a `retyped` build (K16), which routes by charge, the large path
is off. The seal's rules for it are K16's to write.

`dealloc` recognises the area before `heap_drawn_from` with one compare
against a constant: an address at or above `KERNEL_VMAP_BASE` on the
64-bit pair, or below `physmap_base` on ARMv7-A. Those addresses are not
in the direct map, and no other heap memory is outside it. Then it checks
the address is inside the area. An address outside the area is never
freed: it is counted and leaked.

**(c) Contexts, and the deferred, batched shootdown.**

* **Allocation is served in every context.** Interrupt context, with
  interrupts masked, or under a spin lock: the path takes only
  interrupt-masking spin locks (the area's and `FRAMES`), never
  nests them, never sleeps, sends no interrupt and allocates nothing. In
  interrupt context it uses `Floor::Kernel` only. One exception: when the
  address space is exhausted *and* lazy ranges are waiting, only a
  context that may block purges and tries again. Elsewhere the refusal
  stands, and the request goes on as today (null, or the next
  fallback).
* **Free is served in every context, and begins no shootdown.** For each
  page it clears the leaf, with no lock (R6.3 above; a new `Mapper::take_page`
  clears one page-level leaf, returns its frame and prunes nothing), then
  invalidates the entry (`flush_tlb_page`: `invlpg` on x86-64, local; the
  broadcast `tlbi vaae1is` / `TLBIMVAAIS` on Arm, complete on every
  processor at its `dsb ish`). What happens to the frame then is branched
  on one constant, `arch::TLB_FLUSH_IS_BROADCAST`, in one code path
  (R6.1):
  * **Arm (broadcast):** no processor can still hold a translation of
    the frame, so it goes back at once through `deallocate_frames(frame,
    0)` to the reservation its tag names (L.mm.80). H.MEM.17 holds
    literally.
  * **x86-64 (local):** another processor may still hold a translation,
    so the frame is **kept**. It is linked onto the block's chain through
    its own first word, written through the direct map (the chain is
    `vfree_atomic`'s `llist` through the freed memory, and needs no
    storage). The block's record holds the chain's head. The frames go
    back only after the purge's `flush_tlb_everywhere` has returned
    (H.MEM.17, H.MEM.12, L.mm.29), each through `deallocate_frames(frame,
    0)` to its tag's reservation.

  Then the unit block goes on the **lazy list**: an intrusive list
  through the fixed side array, one hold of the area's lock, O(1), with
  no allocation. Its addresses are not reusable yet.
* **The purge** detaches the whole lazy list under the area's lock, drops
  the lock, makes **one** `smp::flush_tlb_everywhere()`, and only then
  gives the kept frames back (x86-64) and the unit blocks to the address
  buddy. It runs only where `sched::may_block()` holds: interrupts
  enabled, preemption count 0, not in an interrupt. So it is never a
  shootdown with interrupts masked or under a spin lock (RD.6 (c), and
  that function's own rule). One purger at a time (an atomic flag). A
  caller that finds the flag taken skips the purge; an allocation that
  needed it gets the refusal and goes on to the next fallback. It is
  started:
  1. by an allocation, where it may block, that finds a purge wanted or
     no address space;
  2. at step 3 of the order above (x86-64, a scoped or job's draw short
     while lazy frames hold its reservation drawn), where it may block;
  3. by **the reaper** (R6.2). A free that leaves more than `LAZY_MAX`
     units waiting sets a purge-wanted flag, an atomic store and nothing
     else, and so does a reservation's unbind while frames are kept: lazy
     frames keep a reservation drawn, and L.mm.80 binds it again only
     once nothing is. Neither purges itself, in any context (F-104, as
     built below). The store allocates nothing and is safe in interrupt
     context.
     `sched::leftovers::nudge`, which already runs at the end of every
     interrupt anywhere in the machine, at the end of every switch and in
     the idle loop's look, with no lock held, wakes the reaper task when
     the flag is set, as it does for a task's leftovers. The reaper is a
     task, so it may block, and it purges. This is Linux's
     `drain_vmap_area_work`. It is an existing task; R5 step 2 does not
     replace it (R6.9).

  `LAZY_MAX` is an eighth of the area's units, about the same in each
  build as Linux's 32 MiB per processor-log. The bound to write in
  MEMORY-AND-TIMING: the purge holds the area's lock for the detach
  (O(1)) and then once per block for its give-back (O(`MAX_ORDER`) each).
  Its one shootdown is `flush_tlb_everywhere`'s existing bound, with
  interrupts enabled. The lazy list holds at most `LAZY_MAX` units plus
  what frees add before the reaper runs. The reaper is woken
  at the machine's next interrupt, switch or halt, the bound
  `sched::leftovers` already states. The frames it holds on x86-64 are at
  most the pages of those units, and the boot report prints the most
  held (`large: ... most held`).
* **What the lazy window leaves.** Nothing, on any architecture: no
  address in the area is handed out again while any processor can hold a
  translation of it, and no frame goes back to the allocator while any
  processor can hold a translation of it (H.MEM.17). The cost on x86-64
  is that a large free's frames come back at the next purge, not at the
  free. Two consequences, both answered:
  * Frame-delta checks: the large path runs only on a buddy refusal, so a
    check on an unfragmented machine never makes a lazy block. A check
    that does make one purges (`mm::large::purge`, where it may block)
    before it reads `free_frames`.
  * A W1 scope's promise: lazy frames keep it drawn. Step 3 of the order
    purges before an overrun is counted, and an unbind asks the reaper
    for a purge.

  Freeing at once on x86-64 stays possible later only as an H.MEM.17
  amendment with a VULNERABILITY-ANALYSIS entry, decided by the customer.
  It is not in this landing.
* **Lazy frames are purged before any kill (R6F.7, x86-64).** The
  watermark counts held frames as used, so a machine could kill a program
  for frames a purge would give back. Where the caller may block, a purge
  runs before step 7's reclaim counts a failure, before step 8's kill,
  before the program-memory OOM kill and before the program floor's
  refusal. MEMORY-AND-TIMING counts the lazily held frames in the
  watermark argument.
* **The chain fails closed (R6F.8).** A write through a stale
  translation could overwrite a freed frame's first word, the chain's
  link. The purge validates each link before it frees: a RAM frame, in
  range, marked held by the large path (set at the free, cleared at the
  give-back), and the chain's length must match the count of frames kept
  into it. A bad link is counted, ends the chain, and its frames are
  leaked, never freed (L.mm.143, asserted 0 at the end of the boot check).
  **As built:** there is one kept chain for the whole area, not one per
  block, so its count lives beside its head in the area's bookkeeping
  rather than in each block's side-array word; a purge detaches both
  together. A truncated or extended chain fails the same comparison.

**(d) Physical consumers (RD.6 (b)).** Audited over ring 0, the kernel's
own files and every linked library: **nothing uses heap memory as
physical memory.** Every ring, descriptor, IOMMU table, ITS table, page
table, page-cache page and buffer shared with ring 3 or a device is built
from `mm::allocate_frames`, `allocate_user_frame` or VMO frames, reached
through `direct_map(frame * PAGE_SIZE)`. The heap only ever holds copies.
The hardware drivers are ring 3. `ferrix-virtio` is linked as a layout
library over frame addresses. The users of heap addresses as addresses
are all inside `mm.rs`: `heap_drawn_from` and `dealloc`, which the
routing above handles; `ReservedPages`, whose order-k first try stays
direct-map; and `mm/reserve.rs`'s `hold_large`, whose reserved section
keeps its direct-map block, since a reserved section is not routed
through the large path. The page-walking readers (`read_kernel_word`,
`backtrace::read_word`) walk the tables, so they work on the area too.
`ram_behind`, `direct_map_ram` and the MCE copy take no heap pointer.
**Decision: R6 stays inside `GlobalAlloc`**, and the opt-in is the other
way round. A consumer that needs a run asks for frames
(`mm::allocate_frames`, `allocate_frame_run`), as every current one
already does. The audit list goes into the landing's commit and into
§2.6's table of physical users.

**(g) Cost.** It is measured in the boot check below, printed and cited
in the commit: 1,000 rounds each of a 32 KiB allocation and free through
the buddy (unfragmented) and through the large path itself (`large::allocate`
and `large::free`, so that the drain of every processor's cache that
precedes it inside the heap is not counted as its cost). The figures
are from x86-64 under KVM and ARMv7-A under TCG, which is QEMU and not
hardware (the DK1 needs the PO). The expected large path is about 8
frame takes, 8 leaf writes, and on free 8 leaf clears with their local
invalidations. That is a few microseconds against the buddy's few hundred
nanoseconds, paid only where the buddy refused. Unchanged, and stated:
the IPC fast path (it does not allocate) and the size classes (the large
path is reached only from the buddy's refusal). `dealloc` gains one
compare against a constant. `check-stack-depth` and `test-ring0-budget`
deltas are named in the commit. `ferrix_kmem::footprint` still charges a
large request as its power-of-two run, which overcharges an area block (by
at most twice). That fails safe, and the text says so.

**(h) ARMv7-A's kernel address space.** The vmap area is 512 MiB, and
64 MiB of it is the fixed windows. The arena is 448 MiB, less up to an
eighth for KASLR. The large area takes at most 128 MiB of it, about a
quarter. That leaves the arena more than 260 MiB for stacks (32 KiB of
span a task with ARMv7-A's three-page lower guard) and device windows.
When the area's buddy has no block and there is nothing to purge, the
answer is a refusal, which falls to the buddy path, and so to today's
behaviour. It is never a panic.

**(i) The check, and its controls.** `mm/large_check.rs`, at stage 9
beside `object/reserve_check.rs`'s heap-scope check, prints its line
first.

**How the buddy is made to refuse (R6.7 (a)-(c)). Departure from CR.4
(i)'s "fragmented by a check-made pattern", for the consultant to
accept or refuse.** Holding every other frame at stage 9 opens a window
in which every other order-above-0 frame request on the machine fails:
secondaries, interrupt handlers and the tasks stages 5 to 8 started are
all running by then. Showing that no infallible request among them can
meet the window means auditing every `allocate_frames(k > 0)` caller,
and the loop costs time on every boot of the row set (test time is
priority one). So the check makes the buddy refuse **the checking task's
runs alone**: a CHECKS-ONLY hook beside `refuse_shared_heap_for`
(`mm::refuse_runs_for(task)`), read in `take_frames` only for an order
above 0, makes every run that task asks for answer `None`, as a buddy
with no free block above order 0 answers. That is the one input the
large path depends on, and every other path on the machine is
untouched. The check first asserts that `allocate_frames(1)` and a
`take_frames` through the reservation's heap answer `None` for it. No
frame is held, so there is no window and nothing to release on a failed
exit. The hook is cleared by a guard on every exit, a failed assertion
included. If this is refused, the fragmentation of the first version
returns with R6.7 (a)-(c) answered: the loop runs before stage 5 starts
the secondaries, and its time on each boot is in the commit.

It does, in order:

1. It measures (g), and R6.5's 4 MiB case (below).
2. It binds a reservation of 64 frames and enters a heap scope on it.
3. It makes the buddy refuse its runs (above), and asserts that it does.
4. It allocates a `Vec<u8>` of 32 KiB, fallibly, and requires:
   * the pointer lies in the area;
   * all 8 of its pages translate to frames tagged with the scope's
     reservation;
   * the page above the block (its guard) does not translate;
   * the reservation's usage rose by 8;
   * no overrun was counted.
5. With interrupts masked, it drops the Vec, and requires:
   * `smp::shootdowns()` did not move;
   * one block waits on the lazy list;
   * on Arm, the reservation's usage is back where it was; on x86-64 it
     is still up by 8 (R6.1).
6. It purges in task context, and requires:
   * on x86-64 with more than one processor, one shootdown begun;
   * an empty lazy list;
   * on x86-64, the reservation's usage back where it was.
7. RD.6 (g)'s case: outside the scope, a 128 KiB request is served from
   the area while the buddy refuses, and freed.
8. R6.3's case: the same 32 KiB request made and freed inside a
   `with_tables` body is served from the area, and the boot goes on.
9. R6.2's case: with interrupts masked, enough 32 KiB blocks are made
   and freed to pass `LAZY_MAX`, and one more request is made, which
   finds the purge wanted. The purge-wanted flag is set, no
   shootdown has begun, and once interrupts are enabled the reaper empties
   the list within a bounded wait (a wait on the state, with a stated
   timeout that fails loudly). Then the unbind's case (F-104, ledger
   8665 D2): with interrupts masked, a 32 KiB block under the threshold
   is made and freed and an entry is bound and unbound. No shootdown has
   begun; on x86-64, where the free's frames are kept, the purge-wanted
   flag is set and the reaper empties the list within the same bounded
   wait; on the Arm pair the flag stays clear and the check purges. And
   the allocation's case, made before step 1: with the threshold lowered
   to 16 units, over 64 rounds of a 32 KiB block made and freed by the
   large path in task context the lazy list never holds more than the
   threshold and one block.
10. It clears the hook and the scope, and requires no frame fewer free
    than before (more is something else on the machine giving frames back,
    printed as other checks print it), an out-of-area `dealloc` count of
    0 (R6.12 (b)), no two owners, no bad link, no frame kept and an empty
    lazy list, and every page-level table of the area still linked.

R6.5's figures come from step 1's 4 MiB case (`n = 1024`), measured on
x86_64 kvm and aarch64 tcg and written into MEMORY-AND-TIMING: the
interrupts-masked time per hold (one frame take, one leaf write), the
total per allocation and per free (on Arm a masked free issues `n`
broadcast invalidations), and the purge's give-back loop. If the
interrupt-context figure is too long, a ceiling on the size served in
interrupt context follows in the same landing.

Controls through `gate.sh control`, all FIRED:

* (C1) the large path removed from `GlobalAlloc`: on "the 32 KiB request
  was not served while the buddy refused";
* (C2) the large path taking `Floor::Kernel` whatever the scope: on "a
  large block's frame was not drawn from the scope";
* (C3) the free making its shootdown in place: on "a masked free began a
  shootdown";
* (C4) the purge giving unit blocks back before its shootdown: caught by
  a host test of the area's state machine in `ferrix-frame` terms (a
  lazy block is not allocatable until the flush callback has returned),
  through `gate.sh control --expect-host`;
* (C5) x86-64: frames freed at the free rather than at the purge: on
  step 5's usage assertion;
* (C6) `dealloc`'s area recognition removed: on the line or stop it
  causes, named in the commit.

Rows: check; test-boot on x86_64 kvm and tcg, aarch64, armv7a and armv7a
`--smp 2`; test-btrfs; test-btrfs-pressure; test-threads;
test-ring0-budget on the tip and on its base.

**Ids (R6.8).** L.mm.141-145 are reserved first, in a docs-only landing
with this revision:

* .141, the path: its threshold, alignment and the order above;
* .142, free routing: the area recognised in `dealloc` before
  `heap_drawn_from`, each frame back to its tag's reservation (L.mm.80),
  an out-of-area address counted and not freed;
* .143, the shootdown order: no address handed out again, and on x86-64
  no frame given back, before the shootdown covering it; the lazy list
  bounded by the kicked purge (parents H.MEM.17 and H.MEM.12);
* .144, no allocation on the mapping path, and the area's exhaustion
  answer; a request refused part way keeps no frame and puts its unit
  block on the lazy list, never straight back;
* .145, the contexts: served in every context, the purge only where the
  caller may block, the large path taking no `TABLES`.

L.mm.81 **is** amended in both clauses: a large block in a reservation's
scope is served by the large path's frames drawn from it, not by that
reservation's heap's pages, and it is freed by the area's routing, not
to "the heap its page's tag names". L.mm.140's statement and criterion
gain the large case (step 4). The landing releases the reservation.

**As built, and the code review (ledger 8583).** The code review found
one blocker, B1: a free of an address the area did not hand out cleared
leaves before it was refused. `large::free` now asks, under the area's
lock and before any leaf is touched, whether the address is the base of
a live block of the request's order (`owns`, `Area::is_live`); anything
else is counted and changes nothing (L.mm.142). The stage 9 check makes
two such frees, one outside the area and one inside a live block, and
requires their mappings untouched. Three more changes from this text:

* **W1-c landed first** (e4950d169). A heap scope now sends a request
  above a page as outside it (L.mm.140, CR.1), so in practice step 1 of
  the order is the job's reservation, not a scope's. The check's drawn
  case therefore runs as a task of a job with a reservation (L.mm.81's
  route) instead of in a scope. Widening the scope to large requests is
  W1-c's follow-up (CR.2). `reservation_grow` (L.mm.146) purges before its
  first kill (R6F.7, 8583 C3).
* **C3's control** fires on smp's own guard, "a shootdown was requested
  with interrupts masked", not on a line of this check: a purge allowed
  where the caller may not block stops the machine there. Since frees
  make no purge (below), what meets that guard is step 9's masked
  request, which finds the purge wanted (`ef30-red3-c1`, ledger 8686 E1).
* **Neither a free nor an unbind purges (F-104, 2026-11-12).** As first
  built, a free past `LAZY_MAX` and a reservation's unbind purged where
  they might block, by `purge`'s own test. A task's last drop frees, and
  lets its job's last slot go and with it the job's reservation, where
  nothing may sleep, and `check-stack-depth` reads any direct way from
  that drop to a shootdown as F-104's line whatever test stands before the
  call: the row was red from 0ec0a3931 on AArch64, then the one image whose
  walk reached the allocator from that drop (F-110: since the walk follows
  x86-64's calls through the GOT and through registers loaded from it, and
  ARMv7-A's calls with their immediate, all three reach it and read no way
  to a shootdown; the units are one source for the three). F-104 itself stayed closed: `purge` refused
  there by its own test. Both now only set
  the purge-wanted flag (`mm::large::retire`, `mm::large::ask_purge_held`),
  and the reaper purges at the machine's next interrupt, switch or halt.
  What it costs: a free in task context past the threshold no longer
  bounds the list itself. An allocation that may block now purges first
  when a purge is wanted (`take_block`), so a task that frees and
  allocates keeps the list at the threshold and one block, as before;
  frees with no allocation between them wait for the reaper, as a free
  that could not block always did. Without that, the check's own 1,000
  rounds left the area full of lazy blocks and its masked 4 MiB request
  was refused in one boot of two. An unbound reservation's entry with
  frames kept waits for the reaper or the next such allocation before
  L.mm.80 binds it again, as it did after an unbind that could not block.
* **F-108 is filed open, Minor** (8583 R4): on x86-64 a large free
  returns while another processor may still translate the freed address
  until the purge, against H.MEM.12's letter. Neither the address nor its
  frame is anyone's meanwhile. Amending H.MEM.12 is the customer's
  decision; the question is with the product owner.

**Landing order against r5-teardown (RD.0, CR.3, R6.9).** R6 changes
`mm.rs` (`GlobalAlloc`, the shared heap's attempt, `take_frames`'s
check hook, the lock doc), a new `mm/large.rs` and its check, `vmap.rs`
(one reserved span, one `Kind`), `arch` (`publish_tables`),
`sched/leftovers.rs` (`nudge` and the reaper's wait also look at the
purge-wanted flag, a few lines) and `ferrix-paging` (`populate`,
`take_page`, with host tests). It does not change `user/`, `user/vmo.rs`,
the frame reference counts, the space lock or `smp.rs`. It only calls `flush_tlb_everywhere`, under that
function's existing contract. R5 step 2 changes those other parts. The
two share only `mm.rs`, in different functions. **Neither waits for the
other.** Whichever lands second rebases. A conflict, or any change by R5
step 2 to `flush_tlb_everywhere`'s calling contract, to
`deallocate_frames` or to the reaper, means that landing re-gates, and
this section's context argument is re-read against it. The purge task is
`sched::leftovers`' reaper, an existing task that R5 step 2 does not own;
if R5 step 2 changes it, R6 lands after R5 step 2. W1-c's widened claim
(CR.2) lands after R6 and names its residual: a scoped large request can
be refused for address space while its reservation still has frames
(the area full, or lazy ranges seen by a caller that may not block).

## 3. Class B: kernel memory a program makes the kernel spend

### 3.1 Evidence

* At the floor the box holds `Slab` 362 MiB. Separately, 3,096 MiB is
  in memory objects that no process maps, most likely the GPU's buffers
  that DXVK uploads through the Intel driver. `/proc/meminfo` counts it
  nowhere (`docs/BTRFS-PRESSURE.md`, "What the box holds during play").
* The machine-wide kill picks "the most resident pages" (`object::oom`).
  The game's kernel heap and its GPU buffers are not in that figure. A
  program that holds 3 GB through the driver is invisible to the
  selection, which can then kill an innocent large process. The AHCI
  driver was killed 0.8 s after the hog in `mp2-btp`.

### 3.2 What exists

* Job quotas (`object/quota.rs`, F-35) charge frames, native objects and
  tasks hierarchically, under `memory.max`.
* `ferrix-kmem` (F-37) charges kernel heap to the job that caused it. Its
  `Charge` token lives inside the object and is used in 38 kernel files:
  pipes, sockets, epoll, inotify, timerfd, procfs buffers, namespaces,
  semaphores and so on.

### 3.2a What Linux does (v7.0)

* `mm/oom_kill.c:oom_badness` scores a task by its resident pages, its
  swap entries and its page tables (`mm_pgtables_bytes`), then adds
  `oom_score_adj` scaled to total memory. `OOM_SCORE_ADJ_MIN` exempts
  the task. Kernel heap charged to the task's memory cgroup is not in
  the per-task score. It counts only inside a memcg OOM, where the
  cgroup's whole charge (`mm/memcontrol.c`, `__GFP_ACCOUNT` /
  `obj_cgroup`) is what is over its limit.
  `select_bad_process` walks the candidates. `__oom_kill_process` kills
  the victim and every process that shares its mm.
* `mark_oom_victim` gives the victim `ALLOC_OOM` access to reserves so
  that its exit can finish (`__gfp_pfmemalloc_flags`). `oom_reaper`
  (`oom_reap_task`, at most `MAX_OOM_REAP_RETRIES`, 10, tries) unmaps
  the victim's anonymous memory without waiting for its exit, so a
  victim stuck on a lock still gives memory back. Ferrix empties a
  victim's space in `object::oom` the same way, but its victim gets no
  reserve. That is the hole `docs/BTRFS-PRESSURE.md` names ("Linux lets
  an OOM victim use reserves to finish").
* So R8's "count charged kernel memory" goes beyond Linux's global
  killer. It is closer to the memcg killer, which kills inside the job
  that is over. The consultant's RD.7 states the constraint: score only
  what the victim's death releases.

### 3.3 The gaps, and their mechanisms

1. **Uncharged heap.** Charges are opt-in per site. The ring-0 audit
   (§2.3) names the sites. R9 makes "charged or argued" a second column
   of the same ratchet. A site that allocates per request either carries
   a `Charge` or says why it is fixed-size.
2. **GPU and display buffers.** An object a driver allocated for a
   client is charged to the driver's job, or to nobody. The render
   interface already knows the client for each buffer. R9 charges the
   client's job through `ferrix-kmem`, the way Linux charges a DRM GEM
   object to the cgroup of the process that made it.
3. **OOM selection.** The victim's score becomes its resident pages plus
   the kernel bytes and buffers its own end would release, divided by the
   page size (R8; RD.7). A charge that outlives the process, such as a
   tmpfs file, is not counted. It is one saturating sum in `object::oom`.
   No new policy enters: it is what "largest" should have meant. The
   victim also gets Linux's `ALLOC_OOM` equivalent: a reserve it may use
   to finish leaving.
4. **Protection is ring 3's.** A typed per-process
   `oom_protect: {none, last, never}` that init sets, and only init may
   set (R8). The service files say `OOMPolicy=` for hyprix, the GPU
   driver, btrfsd and lode. Today the kernel knows only "never pid 1".
   Which services are protected is init's decision.

## 4. Class C: unbounded kernel work done for one program

### 4.1 Evidence

* FX-9008 when Endfield's large Wine process is torn down by a kill or
  an early exit: catch4 #3, #5 and #6, and `eventlog-td-1-develop.txt`.
  It reproduces 3 of 3 on develop. The suspect is GPU teardown. The root
  cause is fault-relanding's.
* The records show no lockup report. A processor spinning with
  interrupts masked would have one, given `ferrix.lockup=on`, which is
  not landed. So the stall is one in which interrupts keep coming and
  user space gets no turn.

### 4.2 The mechanism that turns one long hold into a stall

`crate::sync::SpinLock` is a `PreemptSpinLock`. It raises the processor's
preemption count before it takes its ticket, so the holder stays on its
processor (`ferrix-sync`, `lock`). **A waiter also spins with preemption
off.** One task that holds a widely used lock across a long teardown
(an address space's frames, a VMO's pages, a pin table, a render node's
buffers) therefore makes every other processor that wants that lock
spin. Interrupts still arrive, timers still fire and S4a's heartbeat
still ticks, but no task runs. Init's loop never turns, and FX-9008
fires. This is a hypothesis for 2026-11-08's teardown hang, not a
finding: the hang dump fault-relanding is building will say which lock
it is. The class exists whatever the dump shows.

### 4.2a What Linux does (v7.0)

* **Waiters spin with preemption off there too.** A non-RT
  `spin_lock` disables preemption and then spins on the lock
  (`include/linux/spinlock_api_smp.h`, `__raw_spin_lock`). Ferrix's
  `PreemptSpinLock` behaves the same way. So Linux does not avoid this
  stall through its lock. It avoids it through two rules: nothing long
  is done under a spin lock, and a detector reports any CPU that has not
  scheduled for too long.
* **The soft-lockup detector** (`kernel/watchdog.c`). A per-CPU hrtimer
  (`watchdog_timer_fn`) queues `softlockup_fn` on that CPU's stopper
  thread, the highest-priority task, which touches a timestamp.
  `is_softlockup` reports when the timestamp has not moved for
  `2 * watchdog_thresh` (20 s by default): that CPU has not scheduled.
  The report names the task and prints its stack. `softlockup_panic`
  optionally panics. This is exactly the long-hold class. It is R3's
  model, and R3 needs a scheduling-progress mark rather than a clock read
  on the lock path (RD.2).
* **The hard-lockup detector.** `watchdog_hardlockup_check` sees a
  CPU's hrtimer interrupt count unchanged. The buddy form
  (`kernel/watchdog_buddy.c:watchdog_buddy_check_hardlockup`, the next
  CPU by `watchdog_next_cpu`) is S4a's ring watch, almost one for one.
* **The hung-task detector** (`kernel/hung_task.c`). `khungtaskd` wakes
  every `hung_task_timeout_secs` (120 s by default).
  `check_hung_uninterruptible_tasks` walks every thread, breaking the
  RCU lock every `HUNG_TASK_LOCK_BREAK`. `check_hung_task` reports a
  `TASK_UNINTERRUPTIBLE` task whose switch count has not moved, and
  `hung_task_panic` can turn a count of them into a panic. This is R4's
  stage-1 dump, except that Linux runs it on its own clock rather than
  at a watchdog's pre-timeout.
* **Teardown yields at every level.** `mm/mmap.c:exit_mmap` unmaps
  under `mmap_read_lock` through `mm/memory.c:unmap_vmas`, then frees the
  page tables, then removes the VMAs, with `cond_resched()` per VMA.
  `zap_pmd_range` and `zap_pud_range` `cond_resched()` per entry, and
  `zap_pte_range` drops its PTE spin lock at each PMD. Pages are freed
  in batches through `mm/mmu_gather.c` (`tlb_next_batch`,
  `MAX_GATHER_BATCH_COUNT`), with a `cond_resched()` per batch in the
  free loop. `fs/file.c:close_files` `cond_resched()`s while it closes
  an exiting task's descriptors. Ferrix's `Drop for AddressSpace` has
  none of these points. R5 adds them.

### 4.3 Where today's teardown is unbounded

* `Drop for AddressSpace` (`user/space.rs`) unmaps every region and
  detaches every object in one pass, with no rescheduling point. A 5 GB
  space is about 1.3 M pages.
* A VMO's last drop gives back all its frames. A GPU buffer's goes
  through the render interface's pins.
* An exit closes every descriptor in one pass.
* `munmap` of a huge range is one call.

### 4.4 The mechanisms

1. **Teardown in task context, in bounded steps (R5).** An address
   space, a VMO or a descriptor table is torn down in steps of at most N
   pages or handles (start at 512). Between steps no lock is held and
   the task may be preempted (`sched::yield_now` when a reschedule is
   pending). It runs in the exiting task or the reaper, never under a
   `SpinLock` guard. Linux does the same with `cond_resched()` in
   `unmap_page_range`'s callers and `zap_page_range`'s batching. A
   bound written into `MEMORY-AND-TIMING.md` replaces "the whole space".
2. **A long-hold detector (R3), Linux's soft lockup in Ferrix's
   terms.** Each processor bumps a counter only when its preemption count
   goes from 0 to held, and keeps the `track_caller` line that raised it,
   which FX-0503 already does. There is no clock read on the lock path
   (RD.2). The tick that S4a guarantees reports a raised count whose
   counter has not moved across consecutive ticks. Past about 1 s it
   prints a `held` line with the line, the task and the duration, rate
   limited, from fixed storage. On x86-64 it adds the holder's stack
   through S4a's NMI path. On Arm the line says "no stack". It observes
   only: it never stops or kills. This is the instrument that finds the
   class, and it gives R5 its list.
3. **Spinning waiters are not changed.** A ticket waiter holds its
   ticket from the `fetch_add` on, so it cannot be switched out without
   recreating the convoy that c7413ba fixed (ferrix-sync's own comment;
   the consultant's A1). Linux's non-RT waiters spin with preemption off
   too. The fix is no long holds (R5) and a detector that finds them
   (R3), not a waiter change. A lock redesign would be its own reviewed
   item.

### 4.5 S4a, the parked lockup detector: land it

`s4-lockup` (55ee08384, then c57deecca on the crate split, 397 commits
behind develop) carries S4a: per-processor heartbeats, a ring watch, a 10
s bound and the NMI report. Its S4c part, `/dev/watchdog` and init's
feeder, has landed as POSTMORTEM §6.11. **Recommendation: land S4a**,
rebased and off unless `ferrix.lockup=on`, with the box's command line
asking for it. There are two reasons:

* It catches the stop S4c cannot: every processor masked, so the
  watchdog's own task never runs either. On the box that stop is
  silence until someone cycles the plug.
* Its 1 s tick on every processor is the clock that R3's long-hold
  detector needs. Without it, a tickless idle processor is watched by
  nobody.

It is a core change. It owes more than the design review's D1-D14
(ledger line 2889): the code reviews at lines 3002 and 3018 (R1-R3), 5607
W11-W13, and 8093 SW.12(b), which asks for one watch tick reconciled with
the deadline task that has landed. This document does not land it. R3
rides on top of it.

### 4.6 R5's design: fork and teardown of a big process

**What stopped the box.** Endfield (5 GB resident) forks for its embedded
browser: Wine's `CreateProcess` forks twice, then `execve`s.
`AddressSpace::fork` held the space's `SpinLock`, preemption off, across
work in proportion to the resident pages. That work was `Vmo::fork`, then
`unmap_copied_on_write`, then a shootdown. The game's 40 threads faulting
the same space spun on that lock with preemption off on every other
processor, so all of user space stopped for about 10 s (`ef-load/runs/kb-5`:
the sampler's gap from 179.87 to 189.91 s, and card0 "longest 11589 ms").
The game's own hang watchdog then ended its main thread. Teardowns of big
spaces stalled the same way.

**What it cost, measured.** `cargo xtask stall-probe` (below) on x86-64
under KVM with 4 processors forks a 320 MiB, 40-thread process four times,
then kills it:

| | develop 8ccdab764 | step 1 |
|---|---:|---:|
| another process's worst lateness, forks | 826.9 ms | 79.6 ms |
| the same, teardown phase | 400.5 ms (most likely the post-fork storm's tail) | 0.06-29.9 ms (five runs, below) |
| longest fork | 869 ms | 107-400 ms (the fork thread waits for a processor) |
| `Vmo::fork` under the lock | - | 31-34 ms |
| write-protect under the lock | - | 3.2-4.6 ms |
| single-page shootdowns over 5 ms after the forks | 2,853 | 415 |

Linux 7.0 on the gate host itself (24 processors, the same program run as
`threads-test --stall-probe-only`): forks 3.0 ms, teardown 0.09 ms, the
longest fork 23 ms.

Two causes were not where fault-relanding's notes put them:

* **The unmap was quadratic.** `Mapper::unmap_range_named` pruned after
  each leaf, and `is_empty` read the leaf's table from entry 0 to find it
  not yet empty: about 131,000 reads per full table of 512 pages. That was
  the "full 4-level walk per page, ~5 µs/page". It hit `fork`, `munmap` and
  `Drop` (`unmap_unwalked`) alike. (Develop's 400 ms in the probe's
  teardown phase was most likely not the teardown but the tail of the
  post-fork fault and shootdown storm: with the per-leaf prune put back on
  the branch, the phase measured 0.16 ms. A long hold on one processor
  does not move the probe's ticker on another, so the teardown budget has
  no control that fires (ledger 8421 C1). R5-step2 owes a teardown check
  with the ticker on the teardown's own processor.)
* **The shootdowns after the fork were reuses.** The child exits at once,
  so nearly every page of the parent is held alone again by the time the
  parent writes it. Each write still took the page down, mapped it again and
  ran a single-page shootdown. Forty threads queued on the one shootdown turn.

**What Linux does (v7.0, read, not copied).** `mm/mmap.c:dup_mmap` holds
`mmap_write_lock(oldmm)`, a sleeping lock, for the whole copy. Faulting
threads sleep on it; they do not spin. `mm/memory.c:copy_pte_range` takes
the page-table spin lock per PMD and breaks it every 32 units of progress
when `need_resched()` or `spin_needbreak()`, with `cond_resched()` between
PMDs. `copy_present_ptes` write-protects the source entry in place
(`wrprotect_ptes`), and one `flush_tlb_mm(oldmm)` at the end of `dup_mmap`
covers it all. A write by the only holder reuses the page in place
(`wp_page_reuse` through `ptep_set_access_flags`) with no remote flush;
another processor's stale read-only entry costs it one spurious fault
(`fix_spurious_fault`). On teardown, `mm/mmap.c:exit_mmap` runs
`unmap_vmas` with an `mmu_gather` (`tlb_gather_mmu_fullmm`), and
`zap_pmd_range`/`zap_pud_range` `cond_resched()` per entry. `mm/mmu_gather.c`
frees pages in batches (`tlb_next_batch`, `MAX_GATHER_BATCH_COUNT`) with a
`cond_resched()` per batch.

**Step 1 (this landing): cheap work under the same locks.** It changes no
lock and no order:

1. *Linear prune* (`ferrix-paging`): `unmap_range_named` prunes a leaf's
   tables once the walk leaves that table, or at the range's end, or before
   an error returns. It no longer prunes after every leaf. The tables freed
   and the leaf each is named with are the same as before.
2. *Write-protect in place* (`Mapper::write_protect_range`,
   `mm::write_protect_in`, `AddressSpace::protect_copied_on_write`). Fork
   clears the write permission of every present leaf in a copy-on-write
   region, holes stepped whole, a run in one table read along it. Every
   other bit is kept: the per-architecture `Encoding::write_protected`
   clears x86-64's `RW`, or sets `AP[2]` on AArch64 and ARMv7-A LPAE.
   Then one whole-space shootdown, as before (`TlbPages::everything`).
   Taking a permission away needs no break-before-make on Arm, and Arm's
   invalidation is broadcast. A read after a fork now hits. A write takes
   the copy-on-write branch that a read after the old unmapping led to.
3. *Batched references* (`mm::share_frames`). `Vmo::fork` takes the shared
   pages' references 128 at a time, in one hold of `FRAMES` each, instead
   of one interrupt-masked hold per page. The unwind gives back exactly
   what the new map holds, as before.
4. *Reuse in place* (`Mapper::allow_write`, `mm::allow_write_in`). A
   copy-on-write write whose writer holds the page alone, already mapped
   read-only to that frame, sets the write bit where it is and invalidates
   the faulting processor's own entry under the lock. There is no
   shootdown, because no frame is given back and no translation changes its
   frame. A processor still holding the read-only entry faults once more and
   finds the page writable (`allow_write_in` answers yes for a page already
   writable), as Linux's spurious-fault path does. On x86-64 a page fault
   also drops the faulting entry. On Arm the page invalidation is broadcast.
   The same applies to a private file mapping's shadow page. It is taken
   only when the present leaf decodes to what the old path would install,
   less the write permission: a user page with the region's execute
   permission, on normal memory, not global (`reusable_in_place`). Anything
   else takes the old path. It is L.user.229.
5. *An unmapped page is mapped* (since 2026-11-11, L.user.235, ledger
   8634). A copy-on-write write whose writer holds the page alone and whose
   page has no translation at the faulting address maps it there, writable,
   as any first touch is mapped, and as `write_shadow_copy_on_write` already
   maps a shadow page held alone with no translation (`install_page` with
   no `replace`). That is every page a reserve commits after `mprotect`
   made it writable, which marks the region copy-on-write, and every
   committed page after a protection change, which takes the range's
   translations down. Before, each of these took the old path: a walk of
   every region of the space for other translations of the object's page
   (`forget_in`), then a shootdown of a translation that was never there.
   A Wine process commits all its memory through `mprotect` and holds
   thousands of regions.

   Measured on the N150 box on 2026-11-11 with a static test program
   (`mmap(PROT_NONE)`, `mprotect`, first write; and a written page after
   `mprotect` to read-only and back), Arknights: Endfield running beside
   it, microseconds a page beside 0 and beside 4000 other regions. Before
   (image main-423cc6d75): 3.7 and 11.2 for the reserve, 2.8 and 9.5 for
   the written page. With this step (image main-bdd07bf88, two runs): 2.1
   to 2.3 and 2.2 to 2.6; 1.2 to 1.6 and 1.3 to 1.5. A plain first write
   costs 2.1 to 2.7 there. The game's render thread spent up to 1.9 s at a
   time in these faults in the minute after entering its world (a
   never-land sampler: 60 % of its kernel samples outside a system call in
   `forget_in`). In 180 s from entering the world, one run before: 3
   frames of 500 ms or more, the worst 1.8 s, 12 of 250 ms or more. Two
   runs with this step: none of 500 ms or more, 1 and 2 of 250 ms or more,
   the worst 0.28 and 0.47 s. Frames of 100 to 250 ms are still there (3
   and 42 in the two runs, 24 before); their cause is not this path's.

   Its TLB argument. Nothing is replaced, so nothing is taken down. No
   processor holds a translation of the faulting page that the new one
   could disagree with. An absent entry is never cached. Whatever took the
   page's last translation down counted a shootdown begun and pending
   under the lock hold that took it down, and that shootdown reaches the
   page whether or not it is mapped again meanwhile; until it returns, the
   frame the old entry reaches is kept. A space that is not current on a
   processor flushes what it kept under its PCID when it next runs there,
   by the count begun. On AArch64 and ARMv7-A a permission fault does not
   drop the entry that faulted. A processor that faulted through a stale
   read-only entry while that shootdown is still pending therefore retries
   into it after this step has mapped the page: its second fault finds the
   leaf present and writable and ends in the reuse in place (item 4),
   whose invalidation is broadcast, unless the pending shootdown got there
   before it. That happens once and cannot loop. Every other translation
   of the same object page in this space still reaches the frame the
   object names, because the fault did not change that frame (a copy does,
   and takes the old path), so none has to come down.

   What this rests on, for whoever changes the neighbours: every take-down
   of a leaf in `space.rs` counts a shootdown begun and pending under the
   same lock hold. R5 step 2a and an in-place `mprotect` must keep that or
   re-argue this item.

**Step 1's TLB argument (ledger 8299, A4).** In the reuse the frame does not
change and nothing is given back. This space is the page's only holder, so
the frame goes back to the allocator only through this space's own unmap,
forget or teardown. Each of those takes the translation down and shoots it
down first (`give_back`, `forget_in`, and `Drop`'s rule that no processor is
left in the set). A processor that cached the read-only entry can reach
only that frame, read-only, and a write through it faults. The fault comes
back to the copy-on-write branch, because a region's `cow` is only ever
set, never cleared (`ferrix_vma`), and `allow_write_in` answers yes for a
page that is already writable.

The faulting processor's own entry is invalidated under the lock. On
x86-64, `invlpg` reaches the current PCID only, and a page fault also drops
the entry that faulted (Intel SDM vol. 3A, 4.10.4). A space that is not
current on this processor keeps a stale read-only entry under its PCID, and
that entry faults once more when the space runs, which ends the same way.
On AArch64 and ARMv7-A, `flush_tlb_page` is `dsb ishst` and then a
broadcast invalidation of the page for every ASID (`TLBI VAAE1IS`,
`TLBIMVAAIS`). Changing only a permission needs no break-before-make.

The write-protect and the reuse rewrite a live leaf with a plain read and
write. That is safe because the hardware never writes a leaf this kernel
made. On x86-64 every leaf is written with its accessed bit set, and a
writable one with its dirty bit too. On Arm the access flag is set in every
leaf, and `TCR_EL1.HA` and `HD` are clear.

**Who writes a user frame after a software translation (A5).** A present
read-only leaf is now the usual state after a fork, so each of these paths
has to be checked:

* `AddressSpace::with_page` and `with_present_page`, which carry
  `syscall/uaccess.rs`'s copies and the futex word's. They let a write
  through only when `writable_in_place` holds: a copy-on-write page with
  more than one holder faults first. They read the holders, not the leaf's
  write bit, so a read-only leaf held alone is written through the direct
  map, which is right for a sole holder.
* `lift/core/fill.rs`, the lift personality's fill. It copies the leaf's
  own write bit into the shadow tables, so a read-only leaf stays read-only
  there, and a write faults back to the space.
* Pinned driver buffers (`Vmo::hold`). A held page is copied at fork, never
  shared (L.user.24), so no write-protect reaches it.
* Ring 0 writing through a user address directly. On x86-64 `CR0.WP` is
  set (`arch::kernel_write_protected`), so a read-only leaf faults. On Arm
  the unprivileged accesses are checked against EL0's permissions.

**Where it stands (2026-11-08).** The restricted-window build
(`personality=user`, `restricted_fastpath=on`, `restricted_window=on`,
`--no-claim`) compiled and booted on x86-64 KVM, with xtask exiting 0. That
is build evidence only, not a passing row: a user-personality boot prints
no FERRIX-BOOT-OK, so gate.sh marks it FAILED (ledger 8421 C4). Step 1 was
reviewed OK IF (ledger 8299:
design A1-A6, code B1-B10, budget C1-C3, id D). Step 2's design was
reviewed OK IF E1-E10, and its code review is still owed. The BACKLOG row is
R5-step2. The stall-probe's budgets are interim: 100 ms forking and 50 ms
tearing down (ledger 8299 C, 8324 CT.1), both lowered to 10 ms by step 2.
Five consecutive runs of step 1 (x86-64, KVM, 4 processors) measured the
forks at 76.1, 81.6, 78.4, 73.5 and 68.9 ms and the teardown at 16.0, 29.9,
0.06, 28.4 and 28.2 ms. The teardown's ~28-30 ms has no cause named yet.
Instrumentation that is not on the branch suggested that every
`AddressSpace` and `Vmo` drop took under 3 ms at preemption count 0, and
that the rest is run-queue lateness while the 40 threads exit. That is a
hypothesis, not evidence (ledger 8324 CT.3). Finding the cause is R5-step2's.

**Step 2 (designed, for its own review): fork in bounded steps.** After
step 1, `Vmo::fork` of 320 MiB still holds the space lock for about 33 ms
(400 ns a page, most of it in `fallible::insert`'s reserve per entry).
Faulting threads spin on that lock on every processor, which is the 79.6 ms
left. To bound it:

* *A fork gate.* Under the space lock, fork sets `forking` on the space.
  Every path that takes the space lock to change the space's pages first
  checks the gate under the lock: the faults, `advise`, `with_page`'s retry
  and the native map calls. If the gate is set, the path drops the lock and
  sleeps on the space's wait queue until fork clears it. A path that may not
  sleep (one holding a spin lock) is listed and argued, or refused. Those
  paths are the analogue of Linux's sleeping `mmap_lock`: waiters sleep, so
  the rest of the machine runs.
* *Chunks.* With the gate set and `layout` held (fork already holds it),
  each private object is forked in chunks of at most 512 pages
  (`Vmo::fork_chunk(from) -> next`). Each chunk holds only that object's
  `pages` lock; the space lock is not held. Then the map is cloned and the
  regions write-protected in chunks of 512 pages, each under the space lock.
  The whole-space shootdown follows, then the gate is cleared and the
  waiters are woken. Between chunks no lock is held and the preemption
  count is 0 (RD.5), so the fork thread can be preempted.
* *Consistency.* No fault, `advise` or map call changes the parent while
  the gate is set. The objects' page lists then change only by what does
  not go through the space: `forget_pages`, driven by a shared object,
  which fork does not fork but shares. A user write through a translation
  still writable reaches a frame the child may share. That is Linux's
  window too, until `flush_tlb_mm`. Every write that completes is in the
  child or ordered after the fork, because a write to a page already
  write-protected waits at the gate.
* *Teardown.* `Drop for AddressSpace` already runs with no lock of its own,
  in the reaper or at a retirement. Its unmap is now linear. A step at N
  regions or 512 pages with `sched::yield_now` when a reschedule is pending
  and the preemption count is 0 (RD.5's count-0-only steps) bounds it. Where
  the count is raised (the reaper's preemption window) the drop goes to the
  reaper's own list instead, so it never steps with the count raised.
* *Expected.* The lock holds per chunk are about 0.2 ms, against the 10 ms
  budget step 2 sets for the row.

**Step 3 (later):** an object-level copy-on-write fork (a shared page list
copied on first write), which makes `Vmo::fork` O(1). Not needed for the
budget once step 2 lands.

**The row.** (Budgets: see where it stands, above.) `cargo xtask stall-probe` boots `threads-test` built with
its `stall-probe` feature as init, with 1536 MiB of guest memory. A ticker
process sleeps 1 ms at a time and keeps its longest gap in each phase. A
game process maps and writes 320 MiB, starts 40 writing threads, forks four
times (each child `_exit`s), and is then killed and reaped. The row fails
when the ticker's lateness passes `FORK_BUDGET_US` in the fork phase or
`TEARDOWN_BUDGET_US` in the teardown phase. **Its control is today's code**: on develop 8ccdab764 it
FIRED (`threads: FAILED stall-probe: another process waited past the
budget while a big process forked`, 826,855 µs;
`~/.local/share/ferrix/logs/r5-stall-probe-develop-8ccdab764.log`). After
step 1 a `gate.sh control` that puts back the per-leaf prune and the
unmapping fork must fire again. The same program runs on Linux as
`threads-test --stall-probe-only`, for the comparison.

## 5. Class D: a wedged ring-3 service that everything depends on

### 5.1 Evidence

* catch1 `hang3`: lode, auth and hyprix died of `SIGBUS` and `EIO` after
  the btrfs abort. Init's loop then stalled and FX-9008 reset the box.
* catch4 #4: FX-9008 armed by `fxdiag`, after a `SIGQUIT` to the game's
  wine64-preloader.

### 5.2 What FX-9008 does today

Init feeds `/dev/watchdog` from its event loop. When the deadline
passes, the kernel's watchdog task reports FX-9008 (who armed it, the
timeout, the time since the last feed, and the log's tail) and then
halts, or resets under `ferrix.panic=reset` (`src/kernel/src/watchdog.rs`).
There is one stage, and it is the last one. The report names no blocked
task, so each of today's hangs needed its own diagnostic image.

### 5.2a What Linux does (v7.0)

* `drivers/watchdog/watchdog_dev.c` takes `WDIOC_SETPRETIMEOUT`, and
  `watchdog_pretimeout.c` hands a pre-timeout to a governor.
  `pretimeout_panic.c`'s governor panics, which gives a report before
  the hardware resets; the other governor does nothing. Neither kills a
  culprit.
* The kernel's own detectors (hung task, soft lockup, above) report, and
  panic only when asked to.
* Killing a culprit is ring 3's job on Linux: systemd's per-service
  `WatchdogSec=` with `sd_notify("WATCHDOG=1")` restarts a wedged
  service, and systemd-oomd or earlyoom kill on memory pressure. Stage 2
  below has no Linux counterpart in the kernel. It is a typed mechanism
  that carries out a policy ring 3 registered ahead of time, because a
  stalled user space cannot run its own killer. Its constraints are the
  consultant's RD.4.

### 5.3 A staged response

| Stage | When | Who decides | What happens |
|---|---|---|---|
| 0. Supervision | always | init (ring 3) | (`WATCHDOG-STAGES.md` §5.) Each service's own liveness: a service with `WatchdogSec=` notifies init, as systemd's `sd_notify("WATCHDOG=1")` does. A service that misses is killed and restarted under its `Restart=`. hyprix, the GPU driver and lode get one. A wedged service is then a restart, not a stalled init. |
| 1. Name the stuck | pre-timeout (half the timeout, Linux's `WDIOC_SETPRETIMEOUT`) | the kernel, a fixed report | The **hang dump** (R4): every task blocked for more than 5 s, with its state, its wait channel, the last lock line it took and its kernel stack, plus every processor's preemption-off holder (R3). It goes to the console, pstore and the event log. This is fault-relanding's dump, made a standing mechanism. |
| 2. Kill the culprit | pre-timeout + grace | **policy registered by ring 3 in advance**, carried out by the kernel | At arm time, init names a *sacrifice set*: jobs whose processes may be killed when user space has stopped (the game's session job, say), through a typed ioctl on `/dev/watchdog` that takes a job handle. At pre-timeout the kernel `SIGKILL`s that set. The policy (which jobs) is init's. The kernel has one typed list and one kill. If feeds resume, the record says "recovered by killing job N". |
| 3. Reset | timeout | the kernel | As today: FX-9008, then `ferrix.panic`'s reset. |

Stage 2 helps only when the stall is user space waiting on the culprit.
A kernel lock held by the culprit's own teardown (class C) will not let
the kill finish. That is why R4's dump comes first and R5 is the real
fix. Stage 0 is cheap and is init's alone.

### 5.4 Services must not die of a filesystem's failure

lode, auth and hyprix died of `SIGBUS` because their next text page was
on a root that answered `EIO`. R10: init, lode, hyprix and the GPU
driver lock their text once they have started (`mlockall(MCL_CURRENT)`).
A failing filesystem then cannot fault their code out. `ferrix-vma` keeps
an `mlock`ed state on a region, but whether it keeps file-backed pages
out of reclaim has not been checked, so R10 checks that first. lode
also needs a write path that does not depend on root: the ESP or pstore,
which `ferrix-eventlog` already reads.

## 6. Class E: a filesystem error that becomes a global one

### 6.1 Evidence

catch1 `hang3` and catch2 #1: btrfs aborted a transaction "(out of
memory)", the reload ran out too, and the mount made it final: "every
answer is EIO". Fixed by 197835013, 414134529 and f5cc0e856
(`docs/BTRFS-PRESSURE.md`).

### 6.1a What Linux does

`fs/btrfs/transaction.c:__btrfs_abort_transaction` and
`fs/btrfs/messages.c:__btrfs_handle_fs_error` set
`BTRFS_FS_STATE_ERROR` and make the superblock read-only
(`btrfs_set_sb_rdonly`). New writes then fail with `EROFS`, and reads
keep working. btrfs takes `memalloc_nofs_save` scopes, among them in
`transaction.c`'s commit path, so reclaim never re-enters the
filesystem. Writeback's bios come from mempools
(`mm/mempool.c`), so the path that frees memory does not need fresh
memory. Rules (1) to (3) below are those three.

### 6.2 The rules that close it

1. **An abort costs writes, never reads.** After an aborted transaction a
   mount goes read-only at its last commit, and reading committed data
   keeps working. L.btrfs.122-123 have landed this, and it is Linux's
   behaviour.
2. **"Out of memory" is never final.** A failure caused by memory is
   retried at the next use. Only damage on the disk is final. This has
   landed for the reload. R11 audits the other "unreadable" and
   "poisoned" states in `ferrix-btrfs-vfs`, `ferrix-vfs` and the block
   layer for the same mistake.
3. **Recovery needs memory reserved for it.** A reload that needs memory
   at the moment memory ran out is the trap. R11 gives the reload a
   reservation taken at mount (`mm/reserve.rs`'s reserved sections
   already exist for the item).
4. **One mount's failure stays its own.** A per-mount error state with a
   counter, readable in `/proc/self/mountinfo` and the event log. btrfsd's
   `restarts = 0` is the same rule for `fs=server` (existing BACKLOG row,
   ledger 6933).

## 7. Work items, in order

The points are story points. The first five are the ones to start.

| # | Item | Class | Points | Owner |
|---|---|---|---:|---|
| R1 | **The ring-0 allocation ratchet** (`check-ring0-alloc.py`, §2.5). Done by this landing. | A | 2 | robustness (landed) |
| R2 | **FX-0008 names its caller in the record.** The trace is image-relative in the event log and pstore (ledger 8043 MK.5's row). Without it every FX-0008 costs a code-reading hunt, as #7 did. A core panic-path design goes to the consultant first. | A | 3 | open (MK.5's row) |
| R3 | **The long-hold detector**, riding on S4a's tick, which lands first (§4.4 (2), §4.5). Debug and box images on, the reference configuration off. Also the spin-waiter measurement (§4.4 (3)). | C | 5 + S4a's rebase 3 | open |
| R4 | **The staged watchdog**: the pre-timeout hang dump (stage 1), then the sacrifice set (stage 2), §5.3. The dump first, in its own landing. Designed in `WATCHDOG-STAGES.md` §6-§7; each needs its own design (ledger 8355: not yet). | D | 3 + 5 | watchdog-stages |
| R5 | **Bounded teardown**: address space, VMO, descriptor table and `munmap` in steps of N with no lock held between them (§4.4 (1)). Its order comes from R3's list and fault-relanding's root cause. | C | 8 | step 1 r5-teardown (§4.6); step 2 open |
| R6 | **No contiguous runs for requests above a page**: served from order-0 frames mapped into a kernel area, fallibly, as `kvmalloc`'s fallback (§2.6). Design OK IF R6.1-R6.13 (ledger 8481), revision OK IF R6F.1-R6F.9 (8503); built in `mm::large` (L.mm.141-145). | A | 5 | r6-vmalloc |
| R6b | **A sized, accounted reserve for allocations under a spin lock**, in place of today's implicit headroom (§2.2): `mm/reserve.rs`'s sections generalized, not a second reserve (RD.6 (f)). Split from R6 (ledger 8488 R6.6); owes its own design review before code. | A | 3 | open |
| R7 | **Burn down class A by area**, highest exposure first: the allocations under a lock (tmpfs's maps and `move_child`, then R13's list), `syscall/`, `fs/socket.rs`, `fs/pipe.rs`, `ferrix-vfs` (`namespace`, `tmpfs`, `dentry`), `interfaces/render` and `display` (the GPU ioctls), `ferrix-net`. A crate at 0 gets clippy `disallowed-methods` and leaves the ratchet. Step 1 is a reachability map from the entry points. About 1 point per 40 sites. | A | ~35 | per area owner |
| R8 | **OOM selection counts charged kernel memory and buffers**, plus init's typed `oom_protect` (§3.3 (3), (4)). | B | 5 | open |
| R9 | **Charge the GPU's and the display's buffers to the client's job**, and add a "charged or argued" column to the ratchet (§3.3 (1), (2)). | B | 5 | open, with the intelgpu owner |
| R10 | **Critical services survive a filesystem failure**: `mlockall` in init, lode, hyprix and the GPU driver; lode's root-independent write path; init's `WatchdogSec=` (stage 0) (§5.3, §5.4). Stage 0's `WatchdogSec=` and init's own feeder thread: `WATCHDOG-STAGES.md` §4-§5, watchdog-stages. | D, E | 5 | init, lode; watchdog-stages (stage 0) |
| R11 | **Filesystem failure states audited**: no "final" state caused by memory; a reservation for the reload; per-mount error counters (§6.2). | E | 3 | open, btrfs |
| R12 | **A fault-injection row**: `fallible::inject` armed from a debug-only interface on a workload's process tree (busybox, then a game-shaped load). It passes when every failure becomes `ENOMEM` or a kill and none becomes FX-0008. This finds what the token scan cannot see. | A | 5 | open |
| R13 | **Allocations under a live spin-lock guard, counted at run time** in debug and box images, with their callers, and a row that prints the top sites (§2.4). It feeds R7's order. | A | 3 | open |

## 8. What this document does not own

* **btrfs-pressure**: the specific FX-0008 sites (`openat`'s path clone
  and the 128 KiB one), and the kill that allocates
  (`park/heap-kill-reserve`).
* **fault-relanding**: the teardown hang, the hang dump and the process
  bisect.
* **input-click** and **game-fps**: their own topics.

## 9. Reviews

The certification consultant reviewed `docs/robustness` d0a572964 on
2026-11-08. The review is internal, not independent verification (F-27).
The entry is at ledger line 8202.

* **R1, the ring-0 allocation ratchet: OK IF R1.1-R1.7** (line 8203).
  R1.1 asks for a TOOLS.md row and adds `ring-0 allocation` to the step
  cache's item-keyed list. R1.2 makes `--record` lower only. R1.3 states
  the residual. R1.4 asks for two negative controls through `gate.sh
  control`. R1.5 asks for a `check` row by `gate.sh`, PASSED on the
  landing hash. R1.6 is the patterns-change rule in both docstrings. R1.7
  asks for one author, `ledger 8202` named, and this section.
* **This document: OK IF RD.0-RD.8** (line 8214). That approves the
  direction only. Each of R2, R3, R4 (stage 1 and stage 2 separately),
  R5, R6 and R8 changes core files and gets its own design review before
  code (RD.0). RD.1 to RD.7 give each one's conditions: R2's
  allocation-free bounded walk with image-relative addresses; R3's
  counter with no clock read, observing only; R4's reset not depending on
  the dump, and the sacrifice set's job-handle authority, which refuses
  the root job and the caller's own; R5's count-0-only steps and the
  restated safety argument for `Drop`; R6's opt-in for consumers that
  need physical contiguity; R8's score of only what a death releases.
  RD.8 says no review is owed for R7 in the load ring and in non-item
  crates, for R9, for R10, for R11 on `ferrix-btrfs-vfs`, or for R12 and
  R13 as xtask rows. Review is owed for R11 on the item's btrfs crates
  and for R13's counter in `mm.rs`.
* The consultant checked the document's claims about the code and found
  them true. It corrected four: the waiter change (A1, now §4.4 (3)), S4a's
  distance behind develop and its owed reviews (A2, §4.5), the place of
  MK.5 (A3), and the R2 to R13 range (A4).
* **R6's design (§2.6): OK IF R6.1-R6.13** (ledger 8481, 2026-11-08). The
  shape was accepted: `kvmalloc`'s fallback, tables made at boot, nothing
  allocated on the mapping path, a fixed buddy over address units, a
  batched purge only where the caller may block. R6.1 holds x86-64's
  freed frames until their shootdown (H.MEM.17), R6.2 bounds the lazy
  list, R6.3 closes the self-deadlock on `TABLES`, R6.8 asks for five
  ids. §2.6's revision answers them, with two departures marked for the
  consultant: the large path takes no `TABLES` at all (R6.3), and the
  check makes the buddy refuse the checking task's runs instead of
  fragmenting the machine (CR.4 (i), R6.7 (a)). R6b owes its own review.
* **§2.6's revision: OK IF R6F.1-R6F.9** (ledger 8503, 2026-11-09). All
  three departures accepted. R6F.3, R6F.4 and R6F.7 are in §2.6's text;
  R6F.5 (one refusal decision at steps 5 and 6), R6F.6 (the hook's form,
  the check counting its own purges, a lower threshold for step 9),
  R6F.8 (the chain fails closed) and R6F.9 (the reaper's wait) are
  checked at the code review.
* **R6's code review: NOT YET B1, then OK IF C1-C12** (ledger 8583,
  2026-11-09). B1 (an out-of-area free cleared leaves before its refusal)
  is fixed as above; all four departures of the build were accepted;
  F-108 filed. The follow-up OK on the landing hash is owed.
