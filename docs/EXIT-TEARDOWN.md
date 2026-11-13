# An exit's teardown: never refused, never holding a processor (design (d))

Design note for branch `design/exit-teardown` (silent-reset, 2026-11-13).
This is consultant ledger 9112 Q5's "design (d)": "an exit's teardown shall
take no room it can be refused (forget the views first, as Drop does)". It
follows F-112, which was fixed by `fix/exit-unmap-sort` (ledger 9112, 9327,
9435). It is for design review. No code is written until the verdict.

It answers three questions:

1. Can an exit be refused the room to give its memory back? It can.
2. Can an exit hold a processor or a lock for seconds? It can.
3. Who should do the work?

It also names, measured and not inferred, where the time goes. The time is
not the unmap's walk of its regions and not `Named::of`. It is the kernel
heap's free path, which walks a whole class's free list each time a slab
page empties, with the heap's lock held and interrupts masked (section 2).

## 1. What happens now

A process's end (`Process::release`,
`extensions/personality/linux/src/syscall/process.rs`) empties its user half
with one `AddressSpace::unmap(MMAP_MIN_ADDR, PROGRAM_TOP - MMAP_MIN_ADDR)`.
This happens before any waiter is told, as Linux's `exit_mm` runs before
`exit_notify`. The order matters: a parent's `wait4` must not return while
the child's frames still count against their cgroup (the release's comment:
test-container on AArch64). The unmap is the general one
(`user/space.rs`):

* **Phase one, under the space's spin lock (preemption off):**
  `room_to_count_out` (one `Counting` per region, sorted), `map.remove`
  (allocates one `Unmapping` per region), `count_out_removed`, and
  `take_down` (each region's translations, and one `Freeing` per region).
* **Phase two:** the shootdown.
* **Phase three, `give_back`:**
  * under the lock again: every sole range's frames taken out of its object
    (`take_range`), then `drop_unnamed`, which reads `Named::of` and drops
    every object no region names (`drop_objects`, whose `Arc` drops free
    the objects);
  * after the lock: the taken frames released (`Vmo::retire`).

**Where it can be refused.** Four allocations in phase one can each be
refused: `room_to_count_out`'s list, `map.remove`'s list, the touched ids,
and `Freeing`. The first two refuse the whole unmap. `release` ignores the
answer ("a refusal leaves the pages to go with the space"). The pages then
stay until the last `Arc<AddressSpace>` goes, which is the parent's reap
plus the scheduler's reaper of the dead task. `AddressSpace::drop` then
gives them back, allocating nothing.

**Does a zombie hold its space?**
* When the unmap succeeds, a zombie holds an empty space: the root table,
  an empty map, and the struct.
* When it is refused, the zombie holds every page until it is reaped. The
  memory then comes back as fast as the drop runs, on whichever task drops
  the last reference.

Measured on AArch64 (section 2, last row): the child's memory was still out
at `waitpid`'s return, and came back over the next 10.6 s while the
dropping task ran `Drop`.

## 2. Where the time goes, measured

**How it was measured.** Instrumentation never meant to land (branch
`diag/exit-teardown-profile`, patch
`~/.local/share/ferrix/ef-30fps/exit-teardown/diag-exit-teardown-profile.patch`,
on `fix/exit-unmap-sort` f4dc047d7):
* timers around every step of `unmap` and `give_back`;
* timers inside `drop_objects`, `Vmo::drop`, and the kernel allocator's
  `dealloc`;
* counters for `ferrix-heap`'s `unlink_page` calls and the list links they
  read.

The runs were `test-exit-regions`: release kernel, `ferrix.checks=skip` in
the image's command line, 4 processors, x86-64 under KVM unless named. The
shape was set at build time (`XR_REGIONS`, `XR_KIB`). Logs are in
`~/.local/share/ferrix/ef-30fps/exit-teardown/`.

| | 60,000 x 16 KiB (937 MiB, 2 GiB guest), SIGKILL | 16,384 x 256 KiB (4 GiB, 6 GiB guest) | 36,864 x 256 KiB (9 GiB, 12 GiB guest) |
|---|---:|---:|---:|
| teardown, end to `waitpid` | 3,279-7,141 ms (5 runs, 3 phases) | 18,009 ms | 222,735 ms |
| **kernel heap frees** (count) | **3,218-3,296 ms** (250,188) | **17,850 ms** (232,214) | **222,207 ms** (522,388) |
| slab pages emptied (one list walk each) | 26,983 | 21,684 | 48,794 |
| free-list links read by those walks | 356.5 million | 907.9 million | 4,593.6 million |
| mean walk, interrupts masked | 0.12 ms | 0.82 ms | 4.55 ms |
| space's lock held in all | 3,227-3,331 ms (the frees run in `drop_unnamed`) | 98.6 ms (`take_range` 93.6) | 350 ms (`take_range` 335) |
| `room_to_count_out` + `count_out_removed` | 13-21 + 25-29 ms | 4 + 14 ms | 20 + 47 ms |
| `take_down` (translations) | 6.5-7.1 ms | 13.4 ms | 34.2 ms |
| `map.remove` | 0.7-1.1 ms | 0.2 ms | 0.4 ms |
| shootdown | < 0.1 ms | 0.1 ms | 0.5 ms |
| `Named::of` | 0 (the map is empty by then) | 0 | 0 |
| frames released (`Vmo::drop` or `retire`) | 9 ms (240,794 frames) | 28 ms | 97 ms (about 2.36 M frames) |
| `forget_views` + `detach` | 1.4 + 9.1 ms | 0.3 + 2.6 ms | 0.8 + 7.3 ms |

For the 60,000 row, the parts were measured on the SIGKILL phase of runs
p2-p4 (logs `p2`-`p4`): the frees took 3,218 ms in p3 and 3,296 ms in p4,
and the other rows are the range over p2-p4. The 3,279-7,141 ms teardown
range adds run p1's three phases (exit 4,258, kill 7,141, sigpipe 4,859).

The canary, a 10 ms sleeper in the parent on another processor, woke at
worst 0.1-0.5 ms late in the 60,000 runs, 1.0 ms at 4 GiB and 50.7 ms at
9 GiB.

**The cause is the kernel heap's free path.**
* `ferrix-heap`'s `Heap::deallocate` keeps each size class's free objects
  on one singly linked list. When a free empties a slab page,
  `give_back_if_empty` -> `unlink_page` walks the class's *whole* list to
  take that page's objects off it.
* The walk runs under `HEAP`, an `IrqSpinLock`: interrupts are masked on
  that processor and every other processor's heap use of the shared heap
  waits.
* A teardown frees objects by the hundred thousand and empties slab pages
  by the ten thousand, so its cost is quadratic in the heap's free objects.
  A link costs 9 ns on a short list and 48 ns on a long one, where the list
  spans more memory than the caches hold.

**Without the walks, each teardown above is under half a second:** about
70 ms at 60,000 regions and 0.5 s at 9 GiB, by the rows' sums. So the
3-9 s on x86-64, the 248 s of the 9 GiB hog are this walk. The
10-21 s on AArch64 under TCG take the same path and were not profiled
separately.

The walk was seen before. `bugfix/heap-batched-give-back` (ef-slow,
L.mm.135, ledger 7960 OK IF HB.1-HB.7, unlanded) measured 0.38-0.52 billion
links between Endfield's launch and its title on the N150. It sets emptied
pages aside and walks once per 16. That cuts the total 16-fold but not one
walk's length (section 4.1).

**What is not the cause:**
* The unmap's look per region costs 40-70 ms in all: room, count-out and
  sort.
* `Named::of` costs nothing at an exit, because the map is already empty
  when it is read.
* Freeing the pages costs 9-97 ms.

The 123 us per released view that `rss/anon-merge` measured in a live
space (`docs/ANON-MERGE.md` §3) is a munmap's cost, not an exit's. This
profile does not separate how much of it is `Named::of`'s pass and how much
is the same heap walk (a released view frees its object, and that can
empty a slab page). It is to be measured again after 4.1 (section 7).

**Refusal, measured.** On AArch64 under TCG (2 GiB guest), the child mapped
16 KiB regions until `mmap` refused it with ENOMEM at region 47,083 (the
region map's largest heap request).
* Its exit's unmap answered `Err(OutOfMemory)`. It passed
  `room_to_count_out`, so the refusal was `map.remove`'s list of 47,104
  removals.
* At `waitpid`'s return, MemFree was 1,139,348 kB against 1,983,620 kB
  before: 824 MiB still out.
* `AddressSpace::drop` (47,104 regions, 47,100 objects) then took
  10,625 ms on the dropping task. MemFree was back to 1,983,588 kB 15 s
  later.
* Log: `p5-a64-enomem-serial.log`. The run's own FAILED line is the diag
  program's: the child, out of heap, died of SIGABRT.

## 3. Linux, read first (v7.0, `~/.local/share/ferrix/robustness-ref/linux-v7.0`; read, never copied)

**What Linux does:**
* `kernel/exit.c:exit_mm` drops the task's `mm` before `exit_notify`
  (`exit_mm()` at line 964, `exit_notify` at 988). The last `mmput` runs
  `exit_mmap` on the exiting task. A zombie has `mm == NULL`.
* `mm/mmap.c:exit_mmap`:
  * `unmap_vmas` under `mmap_read_lock` with a full-mm `mmu_gather`;
  * then `MMF_OOM_SKIP` and `mmap_write_lock`;
  * then `free_pgtables`, `tlb_finish_mmu`, and `tear_down_vmas`, which
    calls `remove_vma` per VMA with a `cond_resched()` each.
  * Nothing on this path can fail.
* `mm/memory.c:zap_pte_range` / `zap_pmd_range`:
  * a `need_resched()` break inside the PTE loop;
  * `cond_resched()` per PMD (`} while (pmd++, cond_resched(), addr != end)`);
  * `tlb_flush_mmu` when the gather is full.
* `mm/mmu_gather.c:tlb_next_batch` takes a new batch page with
  `GFP_NOWAIT`. When that is refused, or `MAX_GATHER_BATCH_COUNT` is
  reached, it answers false and the caller flushes and frees what it has
  gathered, then goes on. **A refused allocation makes the teardown do
  smaller pieces, never stop.**
* `mm/oom_kill.c:__oom_reap_task_mm`:
  * runs beside a running `exit_mmap` (both under the read lock), from the
    top of the address space, anonymous and private ranges only, under
    `mmap_read_trylock`;
  * `exit_mmap` sets `MMF_OOM_SKIP` so the killer passes a victim whose
    memory is already going (`OOM-REAPER.md` §2.2).
* `mm/slub.c`:
  * a free goes to its own slab's free list;
  * `__slab_free` -> `slab_empty:` takes an emptied slab off its node's
    partial list with `remove_partial` (`list_del`, O(1)) and
    `discard_slab`s it;
  * a cache keeps `min_partial` slabs.
  * No free ever walks other slabs' objects.

**What Ferrix takes:**
* The order: the exiting task empties the space before the waiter is told
  (as now).
* Smaller pieces, never a stop, on a refused allocation (`tlb_next_batch`'s
  rule).
* A preemption point per bounded unit (`cond_resched` per PMD and per VMA).
* An emptied slab page given back in time independent of the heap's size
  (SLUB's per-slab free list and O(1) `remove_partial`).
* The OOM killer passing a space whose teardown has begun
  (`MMF_OOM_SKIP`).

**What it does not take:**
* The reaper running *beside* an exit on the same space. Ferrix's emptier
  holds one space at a time and the layout lock serialises; R5 2a's
  `teardown_owed` is the `MMF_OOM_SKIP` analogue.
* `mmput_async`, except for a last drop that may not sleep (4.4).
* MMU notifiers, hugetlb and swap: Ferrix has none of them.

## 4. The design

### 4.1 The heap: an emptied slab page goes back without walking the class

A free that empties a slab page takes that page's objects off the free
lists at a cost bounded by the objects of that page, not by the class's
list.

The shape is SLUB's:
* each slab page has its own free list;
* the class keeps a list of pages with free objects (partial pages),
  linked so that a page leaves it in O(1);
* an allocation pops from the class's first partial page;
* a free pushes onto its own page's list;
* the free that empties a page unlinks the page and gives it back, keeping
  the class's last page as L.mm.13 does now.

Where the per-page head and the partial list's links live is the
implementation's choice, to be reviewed with its code. One option is the
per-frame record (two bytes are already used for `slab_free`); another is
a page header, which the crate's head argues against. The 8-byte class
cannot hold two links per object, so a doubly linked class list is not
enough on its own.

The per-processor caches (`HEAP-CACHES.md`) are unchanged.

`bugfix/heap-batched-give-back` (16 emptied pages per walk) becomes
unnecessary. If it lands first it is a stop-gap: it divides the total by 16
but leaves each walk's length, up to 4.5 ms with interrupts masked at 9 GiB.

**Expected effect:** the exit's frees from 13-425 us each to well under a
microsecond, and the teardowns of section 2 to their remaining parts
(about 70 ms and 0.5 s). It is not an exit-only fix. Every mass free in
the kernel walks today, including Endfield's launch on the box (ledger
7960).

### 4.2 An exit takes no room it can be refused

`release` stops calling the general `unmap`. It calls an emptying made for
a space nothing runs in. The release comment already states the
precondition: no thread runs in user mode, and no other process shares the
space. The emptying is `Drop`'s order on a live space:

1. Under the layout and the space's lock, take the map whole: the region
   vector moved out, nothing allocated. R5 2a's
   `ferrix_vma::AddressSpace::take` does this.
2. Forget every object's views first, as `Drop` does
   (`vmo.forget_views(&rss, None)`). With them forgotten, the space's
   counts need no per-region count-out, and so no room for one.
3. Take the translations down and count the shootdown pending, as now.
   After the shootdown, detach each object and drop it, and release the
   page tables (`prune_in`).

The taken regions are walked in place: no `Unmapping` list, no `Freeing`
list, and no touched ids. A refused allocation anywhere below (an object's
own frees allocate nothing) is not possible on this path. Any helper that
does want memory follows `tlb_next_batch`'s rule: with no room it does the
same work one region at a time.

`AddressSpace::drop` stays as it is, and becomes a no-op on an emptied
space. A zombie then holds no page, only the struct, the root and an empty
map, until it is reaped. This is the state Linux's zombie is in, apart from
the root table.

### 4.3 In steps, preemptible

The emptying of 4.2 runs in steps of at most 512 units (leaves or holes of
the walk, regions, objects, frames). Between two steps:
* it holds no spin lock;
* it re-enters at a step point, with preemption count zero, interrupts
  open and not in an interrupt;
* it yields when a reschedule is asked.

This is R5 2a's `user::teardown` and `step_point` (L.user.234, ledger 8522
TD.1-TD.15, code 8559, fix 8603 OK). The layout lock, a sleeping lock, is
held for the whole emptying, so no `mmap` can reuse the range half-way
(L.user.230). The space's lock is held only within a step.

A step's work is bounded once 4.1 has landed. Before 4.1, one object's
free could still walk for milliseconds. Hence the order 4.1, then 4.2,
then 4.3.

### 4.4 Who does the work

**The exiting task, in steps.** This is where Linux does it, and the only
choice that keeps "memory back before the waiter is told" (the cgroup
reason above).

The alternatives were weighed against that:
* **The OOM emptier** (`OOM-REAPER.md`: a task of its own, one space at a
  time machine-wide, served from the reserve, 128-page chunks). It is not
  rejected for speed. It is rejected because it is one slot for the
  machine and is made for victims that cannot run their own exit. It stays
  for those. After 4.2, an ended process holds no pages, so the emptier's
  `oom::ended` finds nothing to hand over. It stays as the defence for a
  space whose emptying was stopped, given up or left part-way.
* **The scheduler's reaper.** It would let `wait4` return with the memory
  still out, which is exactly what the release's comment fixed. It is used
  only where R5 2a uses it (L.user.233): a last drop of a space or object
  where sleeping is forbidden (a raised preemption count) is handed to the
  reaper in a node made with the space, allocating nothing.

## 5. Requirements and checks

**New ids,** reserved before they are written:
* one L.mm (4.1): "a free that empties a slab page shall give it back at a
  cost bounded by that page's objects, not by its class's free list";
* one L.user (4.2): "a process's end shall give back every page of its
  space before its waiter is told, allocating nothing it can be refused".

R5 2a's L.user.230-234 (still reserved to `r5-teardown`) carry 4.3.

**Checks and controls,** each control through `gate.sh control`:
* **4.1, host test (`ferrix-heap`).** A class with a long free list empties
  N pages. The links each give-back reads, counted by the test's
  `Backing`, are at most the page's objects. Control: the class-list walk
  put back -> FIRED (host).
* **4.1, boot check.** A stage case frees 100,000 objects of one class and
  bounds the heap's longest interrupt-masked hold. Control: the same walk.
* **4.2, `test-exit-regions` gains an `enomem` phase** on all three
  architectures. The child maps until refused (AArch64 at 47,083), then
  exits. MemFree at `waitpid`'s return must be within a stated margin of
  before. Control: `release` back on the general `unmap` -> FIRED with the
  phase's line (it fails on AArch64 as section 2 measured).
* **4.2, boot check.** An exit with the kernel heap refusing every
  allocation during the emptying (a check-only refusal gate, CHECKS-ONLY)
  still gives back every frame. Control: the gate's refusal reaching the
  general path.
* **4.3, `test-exit-regions`' canary becomes one sleeper pinned to each
  processor,** as R5 2a's stall-probe has (8426 C1). The present canary,
  on another processor, cannot see a hold on the exiting one. Its worst
  lateness and the teardown time are then asserted, with bounds stated
  from two hosts per CONVENTIONS (the dev and `--release` figures).
  Control: the steps removed (one hold) -> FIRED.

## 6. Landings, sized

| # | What | Where | Size, roughly | Gates and evidence |
|---|---|---|---|---|
| D0 | this note | docs | - | check-docs; design review |
| D1 | 4.1, SLUB-shaped slab pages | `src/lib/kernel/heap` (F-76, unclassified), `mm.rs` | 250-400 lines with host tests | consultant code review; new L.mm id; host-test control; ring0-budget rise; check, check-frames, check-stack-depth, test-boot x4, test-exit-regions x3, the 4 GiB shape once by hand; N150 A/B on Endfield's launch (ledger 7960's figure) if the PO allots the box |
| D2 | 4.2, the exit's emptying | `user/space.rs` (core), `process.rs` (load), `ferrix_vma::take` (from R5 2a) | 150-250 lines | new L.user id; the `enomem` phase and the refusal gate, each with its control; carry-coverage |
| D3 | 4.3 for the exit only: D2's emptying stepped | `user/space.rs`, a small `user/teardown.rs` | 200-300 lines | L.user.230/234 written (reservation shrunk); the pinned canaries and their control; stall-probe |
| D4 | the rest of R5 2a rebased (munmap's `unmap_held`, a stepped last drop and its hand-over, dispose in steps, L.user.231-233, L.object.373) | as R5 2a | its 5,170 lines on 0ec0a3931, 95+ commits stale; conflicts with the rss counts (4120b4def), F-112's heapsort, the OOM emptier, anon-merge | a fresh code review against 8522/8559/8603 |
| D5 | `test-exit-regions` bounds asserted, the 9 GiB shape as a manual row | xtask, `src/tests/exit-regions` (outside the item) | 100 lines | the controls above |

D1 alone takes the measured 3-250 s away. D2 closes the refusal. D3 bounds
what is left per step. D4 extends the steps to every unmap and last drop.
Order: D1 and D2 in parallel (different files), then D3, then D4 and D5.

## 7. Once `rss/anon-merge` lands

anon-merge (L.user.242-244) puts a private anonymous mapping beside its
neighbour in the neighbour's object, so Wine's and a heap's chunks become
few regions and few objects. (d) still has to do all of it:

* **D1.** The 4 GiB row frees 232,214 heap objects for 16,384 objects:
  most frees are the objects' page lists, which scale with the pages, not
  the regions. Merged objects have longer lists, not fewer frees.
* **D2.** A merged object's `take_range` lists 16 bytes a page. A 1 GiB
  object asks for 4 MiB, the largest heap request on AArch64, which is
  refusable. The exit's emptying must not depend on it.
* **D3.** A merged object of gigabytes is one object, and its drop, the
  frames and the page list, must be stepped too. `drop_objects` runs it
  under the space's lock today.
* **The test.** `test-exit-regions` keeps its regions a page apart, so
  anon-merge does not merge them, and the row keeps measuring the worst
  case.
* **`Named::of`.** After D1, the per-release cost anon-merge measured
  (123 us) should be measured again, to see what remains of it once the
  heap walk is gone. With merging, the release that leaves an object
  unnamed becomes rare.

## 8. Not in this design

* A per-processor interrupt stack, and F-111 (no release image judged by
  the stack row).
* Fork's steps: R5 step 2b, the fork gate.
* Why the N150 kept no report of F-112's stop (BACKLOG, owner ef-30fps).
* Whether `lode` should stream its journal (BACKLOG, lode agent).
