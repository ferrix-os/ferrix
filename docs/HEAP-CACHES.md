# Per-processor heap caches

Status: design, OK IF 1-10 (the certification consultant, ledger line 4914,
2026-10-30); revised for conditions 1-4 below.

## 1. Why

Every kernel allocation and free takes one lock, `mm::HEAP`, an `IrqSpinLock`
around the whole heap (`src/kernel/src/mm.rs`, `KernelAllocator`). On the N150
a sampled profile of Arknights: Endfield's shader warm-up (a never-land IP
sampler, branch `diag/kernel-profile-b`) put about a fifth of the game's
kernel time there: `KernelAllocator::alloc` 10 %, and 12 % in reserved
sections, which mask interrupts around one `Arc` or map insert whose
allocation takes the same lock. Four processors allocating at once queue on
one cache line.

## 2. The design

**A cache per processor and size class.** Each processor has, for each of the
nine size classes (`ferrix_heap::CLASS_SIZES`, 8 B to 2 KiB), a stack of at
most `CACHE_DEPTH` (16) free objects of that class: `ferrix_heap::CpuCache`,
safe Rust over `Heap` and `Backing`, host-tested. Requests larger than
`LARGEST_CLASS` go to the heap as now.

**Its own lock.** A processor's cache sits behind an `IrqSpinLock` of its own,
one per processor (`mm::CACHES`, an array of `MAX_CPUS`). The allocator masks
interrupts first, then reads the processor's number, then takes that
processor's cache lock: with interrupts masked the task cannot move between
the read and the lock, and an interrupt handler that allocates cannot come in
on the same processor. Its processor is nearly the only taker, so the lock is
an uncontended atomic; the drain is the only other.

**Allocate.** Under the processor's cache lock: pop an object of the class if
the cache holds one. If not, take the heap lock once and allocate up to
`REFILL` (8) objects of the class: keep all but one and return that one. A
refill that got at least one object has succeeded.

**Free.** Under the processor's cache lock: push the object if the cache has
room. If it is full, take the heap lock once and give back `REFILL` objects of
the class and this one.

**Refused, and the drain (condition 1).** Every heap refusal -- a refill that
got no object, a large request, a reserve's fill (`Reserve::fill` and
`hold_large`, in `reserve::enter`) -- lets go of the heap lock and of this
processor's cache lock, then drains every processor's cache, then tries once
more. The drain is entered with no lock held and holds one cache lock at a
time, and the heap lock under it; a drain entered with a cache lock held is a
stop (`StopCode` for the heap). Only after the retry is refused does the
allocation answer as now: the reserve inside a reserved section
(`reserve::draw`), null otherwise. `reserve::bypassing_heap` is asked first,
as now.

**An injected refusal is not retried (condition 2).** `fallible::inject_once`
with frames refuses one frame allocation by policy; a retry would ask the
policy again and get the frame, and the paths the injection is there to reach
would not be reached. So the allocator reads the injection's failure count
(`fallible::FAILED`) before the attempt; a refusal that raised it is answered
at once, without a drain or a retry. The allocation sweep (`user/alloc_check`)
drains the caches before each run, so a run starts from the same heap whatever
the caches held.

**Counts (condition 3).** A count of the bytes the caches hold, changed under
the cache lock it is about. `heap_allocated()` and `Held::now`'s `heap_bytes`
are the heap's allocated bytes less it. `heap_pages()` and `/proc/meminfo`'s
`Slab` stay the pages the heap holds.

**Lock order.** A processor's cache lock, then the heap lock, then `FRAMES`
(a refill or a large request reaches the buddy). Never the other way. The
drain takes the heap lock under each cache lock in turn, and no cache lock is
held when it starts.

## 3. Bounds (condition 4)

* **Pages held.** At most `CACHE_DEPTH` objects of each of 9 classes per
  processor, 144 objects; in the worst case each keeps a slab page of its own
  from going back: 144 pages, 576 KiB a processor, 2.25 MiB on the N150's
  four. `WATERMARK` (L.mm.71) is at least `WATERMARK_LEAST` and the machine's
  share; 576 KiB a processor is under 1 % of the N150's 16 GiB watermark
  share and is given back by the drain whenever anything is refused, so the
  caches never stand between a program and the watermark's floor.
* **The heap lock's hold.** A refill is up to 8 allocations under one hold,
  each a free-list pop and, when its list is empty, a page take from the
  buddy under `FRAMES`: a 2 KiB class takes 2 objects a page, so at most 4
  page takes a refill. An overflow is 9 frees under one hold; a free that
  empties a page walks the class's free list to unlink it
  (`give_back_if_empty`), as a free does now.
* **A cache lock's hold.** One push or pop, or a refill or overflow above.
* **The drain.** `MAX_CPUS` cache locks, one at a time; under each, at most
  144 frees under the heap lock, each of which may walk a free list as above.
  Interrupts are masked for one processor's share at a time.

## 4. Requirements (reserved as L.mm.93-98)

* L.mm.93: an allocation of a size class takes an object its processor's
  cache holds without the heap lock; an empty cache refills under one hold.
* L.mm.94: a free goes to the processor's cache while it has room; a full
  cache gives back `REFILL` objects and the freed one under one hold.
* L.mm.95: every heap refusal -- a refill, a large request, a reserve's fill
  -- drains every cache and tries once more before it refuses.
* L.mm.96: `heap_allocated()` and the held-memory window's heap bytes are
  the heap's allocated bytes less the cached bytes.
* L.mm.97: the drain leaves every cache empty with its objects in the heap,
  and is entered with no lock held.
* L.mm.98: a refusal the failure injection made is answered without a drain
  or a retry.

## 5. Evidence planned

* Host tests in `ferrix-heap` with negative controls: a pop that takes the
  heap, an overflow that gives back none, a refusal with no drain, a retried
  injection; Miri on the crate.
* Boot checks: the balance on one processor; every cache full and the heap
  exhausted, the drain serving the last allocation; every processor
  allocating and freeing across processors while one is refused; `inject_once`
  with frames reaching its refusal with warm caches. A boot line with the
  worst refill hold and the worst drain hold.
* Controls (`gate.sh control`): a drain with its own cache lock held (the
  stop), `heap_allocated()` not subtracting, `Reserve::fill` with no drain, a
  retried injection.
* The N150 profile again, before and after.

## 6. Where it stands

* 2026-10-30: design OK IF 1-10, ledger line 4914; conditions 1-4 written
  in above.
* 2026-10-30, built: `ferrix_heap::CpuCache` with three host tests;
  `mm::CACHES`, `allocate_through_caches`, the overflow in `dealloc`,
  `heap_or_drain` on every refusal (refill, large request, reserve fill)
  with `fallible::injected_failures` keeping an injected refusal from a
  retry, `drain_caches` with its FX-0012 stop. Boot checks
  (`mm/check.rs::check_heap_caches`, from the stage 9 allocation check):
  the refill, pops, pushes and overflow counted exactly on one processor;
  `heap_allocated()` less the cached bytes; an injected refusal of a page
  answered null with one failure; a page served after a drain with every
  free frame held; every processor allocating, freeing across processors
  and draining at once, finishing with nothing refused and every object
  freed. Boot line, x86-64 under KVM: a refill of 8 objects in 2 us, the
  longest drain 93 us. Controls: a drain inside a cache hold stops
  (FX-0012); `heap_allocated()` not subtracting, a refusal not draining
  and a retried injection each fail their check.
* 2026-10-30, code review against 4914: OK IF H1-H4, ledger line 4959
  (verdict 4964). Rulings: the concurrent check's direct `drain_caches`
  call stands for a refusal's drain, being the same call with nothing
  held; the four controls run on 05830c112 count for a21a595de, the heap
  code differing only by a comment and `#[doc(hidden)]`. Met here: H2, a
  check of its own for a reserve's fill and its large hold
  (`reserve_refusals_drain_every_cache`: one 2 KiB object taken out of the
  reserve and every free one of the class out of the heap, or a page held,
  with every free frame held), with a control for each; H3, MEMORY-AND-TIMING
  §1.8 no longer says the caches never stand between a program and the
  watermark (a program's frame refusal does not drain them), and says a
  section's drain runs masked across every cache. Not owed and noted: the
  injection's count is global, so one armed on another processor keeps a
  refusal here from its retry (a comment at `heap_or_drain`); its doc names
  the masked call from `reserve::enter`.
* N150, Endfield's warm-up, the never-land sampler (performance evidence,
  not certification evidence): before (develop with the btrfs and reserve
  fixes, e0cdb14a1) against after (this branch with the resident-set fix):
  the kernel's share of the game's samples 48.5 % -> 43.6 %; the
  allocator's (`alloc` and `dealloc`) about 77 -> 25 samples a second
  (-67 %); the interrupt-masked reserved sections about 76 -> 37 a second
  (-51 %). Launch to title 213 s -> 198 s, inside the 30 s the title's
  detection can miss by. The two windows are not the same phases of the
  warm-up to the second; the shares are the figure to read.
